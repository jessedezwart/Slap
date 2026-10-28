//! The player's playback controls.

use std::sync::Arc;

use twilight_model::{
    application::interaction::Interaction,
    id::{Id, marker::MessageMarker},
};

use super::{CommandError, caller_channel};
use crate::{
    session::{Control, SessionError},
    state::State,
};

/// The component a button was pressed on.
fn component_message_id(interaction: &Interaction) -> Result<Id<MessageMarker>, CommandError> {
    let message = interaction.message.as_ref().ok_or(SessionError::Ended)?;
    Ok(message.id)
}

pub(super) async fn apply_control(
    state: &Arc<State>,
    interaction: &Interaction,
    control: Control,
) -> Result<(), CommandError> {
    let (guild_id, channel_id) = caller_channel(state, interaction)?;
    // Send the control to the session
    state
        .sessions
        .control(
            guild_id,
            channel_id,
            component_message_id(interaction)?,
            control,
        )
        .await?;
    Ok(())
}
