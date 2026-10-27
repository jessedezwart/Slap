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

use crate::{discord::state::State, voice::session::QueueMode};

/// Identifies both the modal `/play` opens and the submission it sends back.
pub(super) const PLAY_MODAL: &str = "play:modal";

pub fn play_command() -> Command {
    CommandBuilder::new(
        "play",
        "Play a track or add a playlist to your voice channel",
        CommandType::ChatInput,
    )
    .option(
        StringBuilder::new("url", "An HTTP(S) track or playlist link")
            .required(false)
            .max_length(1024),
    )
    .build()
}

/// What the user passed to `/play` directly.
/// `None` without a link, so the form opens instead.
pub(super) fn command_input(data: &CommandData) -> Option<PlayInput> {
    let value = |name: &str| {
        data.options
            .iter()
            .find(|option| option.name == name)
            .map(|option| &option.value)
    };
    let url = match value("url") {
        Some(CommandOptionValue::String(url)) if !url.trim().is_empty() => url.trim().to_owned(),
        _ => return None,
    };
    Some(PlayInput {
        url,
        // The placement only matters next to an existing queue, which is what
        // the form and the Add button are for.
        mode: QueueMode::Append,
    })
}

/// The form `/play` opens: the link, plus where it goes in the queue.
#[allow(deprecated)] // TextInput::label is superseded by the Label component.
fn play_modal() -> InteractionResponse {
    let url = Component::Label(Label {
        id: None,
        label: "Track or playlist link".into(),
        description: Some("An HTTP(S) track or playlist link".into()),
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
    // Labels wrap one component each; action rows are accepted for safety.
    fn walk(components: &[ModalInteractionComponent], input: &mut PlayInput) {
        for component in components {
            match component {
                ModalInteractionComponent::Label(label) => {
                    walk(std::slice::from_ref(&label.component), input);
                }
                ModalInteractionComponent::ActionRow(row) => walk(&row.components, input),
                ModalInteractionComponent::TextInput(text) if text.custom_id == "url" => {
                    input.url = text.value.trim().to_owned();
                }
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
    }
    walk(&data.components, &mut input);
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
#[path = "../../../tests/discord/commands/form.rs"]
mod tests;
