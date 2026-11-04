//! The player's buttons: what each one does and how it is drawn.

use twilight_model::channel::message::{
    Component, EmojiReactionType,
    component::{Button, ButtonStyle},
};

use crate::session::Control;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    OpenForm,
    Control(Control),
    Stats,
}

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
        action: Action::OpenForm,
        custom_id: "player:add",
        label: "Add",
        emoji: "➕",
    },
    PlayerButton {
        action: Action::Stats,
        custom_id: "player:stats",
        label: "Stats",
        emoji: "📊",
    },
];

/// What a press on the button with `custom_id` asks for.
pub fn button_action(custom_id: &str) -> Option<Action> {
    BUTTONS
        .iter()
        .find(|button| button.custom_id == custom_id)
        .map(|button| button.action)
}

pub(super) fn button(action: Action, disabled: bool) -> Component {
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
#[path = "../../../tests/discord/player/buttons.rs"]
mod tests;
