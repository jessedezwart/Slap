use std::sync::Arc;

use tracing::info;
use twilight_gateway::{Config as GatewayConfig, Intents, Shard};
use twilight_http::Client as HttpClient;
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ApplicationMarker, UserMarker},
    },
};

use crate::{BoxError, discord::interactions};

const INTENTS: Intents = Intents::GUILDS.union(Intents::GUILD_VOICE_STATES);

/// An authenticated Discord connection, with shards built but not yet running.
pub struct Discord {
    pub twilight_client: Arc<HttpClient>,
    pub bot_application_id: Id<ApplicationMarker>,
    pub bot_user_id: Id<UserMarker>,
    pub shards: Vec<Shard>,
}

impl Discord {
    /// Authenticate, register commands and create the shards.
    pub async fn connect(token: String) -> Result<Self, BoxError> {
        // Init HTTP client.
        let twilight_client = Arc::new(
            HttpClient::builder()
                .token(token.clone())
                // Mentions shouldn't cause pings.
                .default_allowed_mentions(AllowedMentions::default())
                .build(),
        );

        let bot_application_id = twilight_client
            .current_user_application()
            .await?
            .model()
            .await?
            .id;
        let bot_user_id = twilight_client.current_user().await?.model().await?.id;

        info!(%bot_application_id, %bot_user_id, "authenticated");

        // Register play command
        let commands = [interactions::play_command()];
        twilight_client
            .interaction(bot_application_id)
            .set_global_commands(&commands)
            .await?;
        let names: Vec<_> = commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        info!(commands = ?names, "registered commands");

        let gateway_config = GatewayConfig::new(token, INTENTS);
        let shards: Vec<_> =
            twilight_gateway::create_recommended(&twilight_client, gateway_config, |_, builder| {
                builder.build()
            })
            .await?
            .collect();

        info!(shards = shards.len(), "created shards");

        Ok(Self {
            twilight_client,
            bot_application_id,
            bot_user_id,
            shards,
        })
    }
}
