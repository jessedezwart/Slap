//! The player's playback controls.

use std::sync::Arc;

use twilight_model::{
    application::interaction::Interaction,
    id::{Id, marker::MessageMarker},
};

use super::{CommandError, Reply, caller_channel};
use crate::{
    session::{Control, SessionError},
    state::State,
};

/// The player message a button was pressed on.
fn player_message(interaction: &Interaction) -> Result<Id<MessageMarker>, CommandError> {
    let message = interaction.message.as_ref().ok_or(SessionError::Ended)?;
    Ok(message.id)
}

/// Nothing to say on success: the player shows the change.
pub(super) async fn control_playback(
    state: &Arc<State>,
    interaction: &Interaction,
    control: Control,
) -> Result<Reply, CommandError> {
    let (guild_id, channel_id) = caller_channel(state, interaction)?;
    state
        .sessions
        .control(guild_id, channel_id, player_message(interaction)?, control)
        .await?;
    Ok(Reply::default())
}
