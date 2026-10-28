use super::*;

use twilight_model::id::Id;

#[test]
fn modal_submissions_default_to_appending() {
    use twilight_model::application::interaction::modal::{
        ModalInteractionLabel, ModalInteractionStringSelect, ModalInteractionTextInput,
    };

    let label = |component| {
        ModalInteractionComponent::Label(ModalInteractionLabel {
            id: 0,
            component: Box::new(component),
        })
    };
    let url = |value: &str| {
        label(ModalInteractionComponent::TextInput(
            ModalInteractionTextInput {
                custom_id: "query".into(),
                id: 1,
                value: value.into(),
            },
        ))
    };
    let select = |custom_id: &str, values: Vec<String>| {
        label(ModalInteractionComponent::StringSelect(
            ModalInteractionStringSelect {
                id: 2,
                custom_id: custom_id.into(),
                values,
            },
        ))
    };
    let mode = |values: Vec<String>| select("mode", values);
    let data = |components| ModalInteractionData {
        components,
        custom_id: PLAY_MODAL.into(),
        resolved: None,
    };

    // An untouched select submits no value, which must leave the import at
    // the back of the queue.
    let input = play_input(&data(vec![url("https://example.com/a")]));
    assert_eq!(input.query, "https://example.com/a");
    assert_eq!(input.mode, QueueMode::Append);
    for (value, expected) in [
        ("append", QueueMode::Append),
        ("next", QueueMode::Next),
        ("replace", QueueMode::Replace),
        ("nonsense", QueueMode::Append),
    ] {
        let input = play_input(&data(vec![
            url("https://example.com/a"),
            mode(vec![value.into()]),
        ]));
        assert_eq!(input.mode, expected);
    }
}

#[test]
fn slash_command_only_runs_with_input() {
    use twilight_model::application::interaction::application_command::CommandDataOption;

    let data = |options: Vec<(&str, &str)>| CommandData {
        guild_id: None,
        id: Id::new(1),
        name: "play".into(),
        kind: CommandType::ChatInput,
        options: options
            .into_iter()
            .map(|(name, value)| CommandDataOption {
                name: name.into(),
                value: CommandOptionValue::String(value.into()),
            })
            .collect(),
        resolved: None,
        target_id: None,
    };

    // Without input the command has nothing to run, so it opens the form.
    assert!(command_input(&data(vec![])).is_none());
    assert!(command_input(&data(vec![("query", "   ")])).is_none());

    let input = command_input(&data(vec![("query", "https://example.com/a")])).unwrap();
    assert_eq!(input.query, "https://example.com/a");
    assert_eq!(input.mode, QueueMode::Append);
}
