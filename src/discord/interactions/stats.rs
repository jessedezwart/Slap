//! The Stats button: a guild's all-time play stats.

use std::sync::Arc;

use twilight_model::{
    application::interaction::Interaction,
    channel::message::{AllowedMentions, MessageFlags},
    http::interaction::{InteractionResponse, InteractionResponseType},
};
use twilight_util::builder::InteractionResponseDataBuilder;

use crate::{BoxError, bot::BotContext, discord::player};

/// Entries shown in each ranking.
const TOP_LEN: usize = 5;

/// Answer straight away with the stats; reading them takes no noticeable time.
pub(super) async fn show(
    state: &Arc<BotContext>,
    interaction: &Interaction,
) -> Result<(), BoxError> {
    let summary = match (&state.history, interaction.guild_id) {
        (Some(history), Some(guild_id)) => {
            let history = Arc::clone(history);
            tokio::task::spawn_blocking(move || history.summary(guild_id, TOP_LEN)).await?
        }
        _ => None,
    };
    let data = match summary {
        Some(summary) => InteractionResponseDataBuilder::new()
            .components(player::stats_message(&summary))
            .flags(MessageFlags::IS_COMPONENTS_V2),
        None => InteractionResponseDataBuilder::new()
            .content("Stats are not available on this bot.")
            .flags(MessageFlags::EPHEMERAL),
    };
    state
        .http
        .interaction(state.application_id)
        .create_response(
            interaction.id,
            &interaction.token,
            &InteractionResponse {
                kind: InteractionResponseType::ChannelMessageWithSource,
                // Users are listed by mention, which must not ping them.
                data: Some(data.allowed_mentions(AllowedMentions::default()).build()),
            },
        )
        .await?;
    Ok(())
}
