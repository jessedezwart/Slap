//! The `/play` command and the form it and the Add button open.

use std::sync::Arc;

use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            Interaction,
            application_command::{CommandData, CommandOptionValue},
            modal::{ModalInteractionComponent, ModalInteractionData},
        },
    },
    channel::message::{
        Component,
        component::{
            Label, SelectMenu, SelectMenuOption, SelectMenuType, TextInput, TextInputStyle,
        },
    },
    http::interaction::{InteractionResponse, InteractionResponseType},
};
use twilight_util::builder::{
    InteractionResponseDataBuilder,
    command::{CommandBuilder, StringBuilder},
};

use crate::{session::QueueMode, state::State};

/// Identifies both the modal `/play` opens and the submission it sends back.
pub(super) const PLAY_MODAL: &str = "play:modal";

pub fn play_command() -> Command {
    CommandBuilder::new(
        "play",
        "Play a track or add a playlist to your voice channel",
        CommandType::ChatInput,
    )
    .option(
        StringBuilder::new("url", "A track or playlist link")
            .required(false)
            .max_length(1024),
    )
    .build()
}

/// What the user passed to `/play` directly.
pub(super) fn command_input(data: &CommandData) -> Option<PlayInput> {
    let option = data.options.iter().find(|option| option.name == "url");
    let url = match option.map(|option| &option.value) {
        // Open the form when no link supplied.
        Some(CommandOptionValue::String(url)) if !url.trim().is_empty() => url.clone(),
        _ => return None,
    };
    Some(PlayInput {
        url,
        mode: QueueMode::Append,
    })
}

/// The form `/play` opens
#[allow(deprecated)] // The `SelectMenu` component is deprecated, but has no replacement yet.
fn play_modal() -> InteractionResponse {
    let url = Component::Label(Label {
        id: None,
        label: "Track or playlist link".into(),
        description: Some("Can be Spotify, YouTube, SoundCloud etc.".into()),
        component: Box::new(Component::TextInput(TextInput {
            id: None,
            custom_id: "url".into(),
            label: None,
            max_length: Some(1024),
            min_length: Some(1),
            placeholder: Some("https://…".into()),
            required: Some(true),
            style: TextInputStyle::Short,
            value: None,
        })),
    });
    let mode = Component::Label(Label {
        id: None,
        label: "If something is already playing".into(),
        description: Some("Where this import goes relative to the current queue".into()),
        component: Box::new(Component::SelectMenu(SelectMenu {
            id: None,
            channel_types: None,
            custom_id: "mode".into(),
            default_values: None,
            disabled: false,
            kind: SelectMenuType::Text,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(vec![
                SelectMenuOption {
                    default: true,
                    description: Some("Play it after everything already queued".into()),
                    emoji: None,
                    label: "Add to the queue".into(),
                    value: "append".into(),
                },
                SelectMenuOption {
                    default: false,
                    description: Some("Jump the queue, after the current track".into()),
                    emoji: None,
                    label: "Play next".into(),
                    value: "next".into(),
                },
                SelectMenuOption {
                    default: false,
                    description: Some("Clear the queue and start this instead".into()),
                    emoji: None,
                    label: "Replace the queue".into(),
                    value: "replace".into(),
                },
            ]),
            placeholder: Some("Add to the queue".into()),
            required: Some(false),
        })),
    });
    InteractionResponse {
        kind: InteractionResponseType::Modal,
        data: Some(
            InteractionResponseDataBuilder::new()
                .custom_id(PLAY_MODAL)
                .title("Play")
                .components([url, mode])
                .build(),
        ),
    }
}

/// What the user typed into the modal.
pub(super) struct PlayInput {
    pub url: String,
    pub mode: QueueMode,
}

pub(super) fn play_input(data: &ModalInteractionData) -> PlayInput {
    let mut input = PlayInput {
        url: String::new(),
        mode: QueueMode::Append,
    };
    // Loop over the components to find the user inputs.
    for component in &data.components {
        // The fields are wrapped in a label
        let ModalInteractionComponent::Label(label) = component else {
            continue;
        };
        match &*label.component {
            // URL field
            ModalInteractionComponent::TextInput(text) if text.custom_id == "url" => {
                input.url.clone_from(&text.value);
            }
            // Queue mode field
            ModalInteractionComponent::StringSelect(select) if select.custom_id == "mode" => {
                input.mode = match select.values.first().map(String::as_str) {
                    Some("next") => QueueMode::Next,
                    Some("replace") => QueueMode::Replace,
                    _ => QueueMode::Append,
                };
            }
            _ => {}
        }
    }
    input
}

/// A slash `/play` answers with the form instead of a deferred reply.
pub(super) async fn open_modal(
    state: &Arc<State>,
    interaction: &Interaction,
) -> Result<(), twilight_http::Error> {
    state
        .http
        .interaction(state.application_id)
        .create_response(interaction.id, &interaction.token, &play_modal())
        .await
        .map(|_| ())
}

#[cfg(test)]
#[path = "../../../tests/discord/commands/play_modal.rs"]
mod tests;
