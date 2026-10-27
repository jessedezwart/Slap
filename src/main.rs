mod audio;
mod config;
mod discord;
mod media;
mod session;
mod state;

use std::{collections::HashMap, process::ExitCode, sync::Arc};

use songbird::{Config as SongbirdConfig, Songbird, shards::TwilightMap};
use tokio::{signal, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_gateway::{Config as GatewayConfig, Intents};
use twilight_http::Client as HttpClient;
use twilight_model::channel::message::AllowedMentions;

use crate::{
    audio::source::AudioContext,
    config::Config,
    discord::{commands, events::run_shard},
    media::{
        importer::Importer,
        search_cache::SearchCache,
        spotify::SpotifyService,
        ytdlp::{YtDlpClient, YtDlpResolver},
    },
    session::SessionManager,
    state::State,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const INTENTS: Intents = Intents::GUILDS.union(Intents::GUILD_VOICE_STATES);

fn main() -> ExitCode {
    let dotenv = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new(
                "info,songbird::tracks::queue=warn,songbird::driver::tasks::events=warn,\
                 songbird::events::store=warn",
            )
        }))
        .init();

    info!("starting...");

    match dotenv {
        Ok(path) => info!(path = %path.display(), "loaded .env"),
        // No `.env` is the normal case in production.
        Err(error) if error.not_found() => {}
        Err(error) => warn!(%error, "could not read .env; using the environment as-is"),
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            error!(%error, "invalid configuration");

            return ExitCode::FAILURE;
        }
    };

    // Sized from `TOKIO_WORKER_THREADS`, or one worker per core.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            error!(%error, "could not start the tokio runtime");

            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(%error, "fatal error");

            ExitCode::FAILURE
        }
    }
}

async fn run(config: Config) -> Result<(), BoxError> {
    let bot = Bot::start(config).await?;
    info!("ready");

    signal::ctrl_c().await?;
    info!("shutting down");

    bot.shutdown().await;

    Ok(())
}

/// A running bot: everything `start` built that `shutdown` takes down again.
struct Bot {
    state: Arc<State>,
    shutdown: CancellationToken,
    shards: Vec<JoinHandle<()>>,
}

impl Bot {
    /// Authenticate, register commands and connect every shard.
    async fn start(config: Config) -> Result<Self, BoxError> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let http = Arc::new(
            HttpClient::builder()
                .token(config.token.clone())
                .default_allowed_mentions(AllowedMentions::default())
                .build(),
        );

        let application_id = http.current_user_application().await?.model().await?.id;
        let user_id = http.current_user().await?.model().await?.id;

        info!(%application_id, %user_id, "authenticated");

        http.interaction(application_id)
            .set_global_commands(&[commands::play_command()])
            .await?;

        let gateway_config = GatewayConfig::new(config.token.clone(), INTENTS);
        let shards: Vec<_> =
            twilight_gateway::create_recommended(&http, gateway_config, |_, builder| {
                builder.build()
            })
            .await?
            .collect();

        info!(shards = shards.len(), "starting shards");

        let senders: HashMap<_, _> = shards
            .iter()
            .map(|shard| (shard.id().number(), shard.sender()))
            .collect();

        let songbird = Arc::new(Songbird::twilight_from_config(
            Arc::new(TwilightMap::new(senders)),
            user_id,
            SongbirdConfig::default().preallocated_tracks(2),
        ));

        let sessions = SessionManager::new(Arc::clone(&songbird));

        let search_cache = config
            .search_cache_path
            .as_deref()
            .and_then(SearchCache::open_or_skip);

        // Importing and resolving run the same program, so they share a client.
        let ytdlp = YtDlpClient::new(config.ytdlp);
        let resolver = YtDlpResolver::new(ytdlp.clone(), search_cache);
        let importer = Importer::new(SpotifyService::new(config.spotify)?, ytdlp);

        let state = Arc::new(State {
            player_tasks: tokio_util::task::TaskTracker::new(),
            http: Arc::clone(&http),
            discord_cache: Arc::new(
                DefaultInMemoryCache::builder()
                    .resource_types(ResourceType::VOICE_STATE)
                    .build(),
            ),
            songbird,
            sessions,
            importer,
            audio: Arc::new(AudioContext {
                http: reqwest::Client::builder().user_agent("slap").build()?,
                url_resolver: resolver,
                buffer: config.buffer,
            }),
            application_id,
            user_id,
        });

        let shutdown = CancellationToken::new();

        let shards: Vec<_> = shards
            .into_iter()
            .map(|shard| tokio::spawn(run_shard(Arc::clone(&state), shard, shutdown.clone())))
            .collect();

        Ok(Self {
            state,
            shutdown,
            shards,
        })
    }

    async fn shutdown(self) {
        let Self {
            state,
            shutdown: cancellation_token,
            shards,
        } = self;
        info!("leaving voice channels");
        for guild_id in state.sessions.known_guilds() {
            state.sessions.teardown(guild_id, "shutdown").await;
        }

        cancellation_token.cancel();

        for shard in shards {
            let _ = shard.await;
        }

        state.player_tasks.close();
        // Wait till player tasks are finished or timeout.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(12),
            state.player_tasks.wait(),
        )
        .await;
    }
}
