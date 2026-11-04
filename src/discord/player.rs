//! The player message: its buttons, and the task that keeps it current.

mod buttons;
mod task;
mod text;
mod view;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use twilight_model::{
    channel::{Message, message::Component},
    id::{Id, marker::GuildMarker},
};

pub use self::buttons::{Action, button_action};
use self::{
    task::{PlayerTask, delete_player},
    view::View,
};
use crate::{bot::BotContext, history::GuildSummary};

/// The message the Stats button answers with.
pub fn stats_message(summary: &GuildSummary) -> Vec<Component> {
    View::stats(summary).components
}

pub async fn start(
    state: Arc<BotContext>,
    guild_id: Id<GuildMarker>,
    cancel: &CancellationToken,
    message: Message,
) {
    let Some(session) = state
        .sessions
        .claim_player_component(guild_id, cancel, message.id)
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
