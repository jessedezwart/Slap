//! The player message: its buttons, and the task that keeps it current.

mod buttons;
mod task;
mod text;
mod view;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use twilight_model::{
    channel::Message,
    id::{Id, marker::GuildMarker},
};

pub use self::buttons::{Action, button_action};
use self::task::{PlayerTask, delete_player};
use crate::bot::BotContext;

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
