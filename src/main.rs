mod audio;
mod config;
mod discord;
mod media;
mod voice;

use std::{collections::HashMap, process::ExitCode, sync::Arc};

use songbird::{Config as SongbirdConfig, Songbird, shards::TwilightMap};
use tokio::{signal, sync::watch, task::JoinHandle};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_gateway::{Config as GatewayConfig, Intents};
use twilight_http::Client as HttpClient;

use crate::{
    audio::source::AudioContext,
    config::Config,
    discord::{commands, events::dispatcher, state::State},
    media::{
        PLAYLIST_LIMIT, importer::Importer, search_cache::SearchCache, spotify::SpotifyService,
        ytdlp,
    },
    voice::session::SessionManager,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const INTENTS: Intents = Intents::GUILDS.union(Intents::GUILD_VOICE_STATES);

fn main() -> ExitCode {
    let dotenv = dotenvy::dotenv();
    // After `.env`, so a `RUST_LOG` set there applies.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
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
    shutdown_tx: watch::Sender<bool>,
    dispatchers: Vec<JoinHandle<()>>,
}

impl Bot {
    /// Authenticate, register commands and connect every shard.
    async fn start(config: Config) -> Result<Self, BoxError> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let http = Arc::new(HttpClient::new(config.token.clone()));

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

        let (ytdlp, resolver) = ytdlp::build(config.ytdlp.clone(), search_cache);
        let spotify = SpotifyService::new(config.spotify.clone(), PLAYLIST_LIMIT)?;
        let importer = Importer::new(spotify, ytdlp);

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
                resolver,
                buffer: config.buffer,
            }),
            application_id,
            user_id,
        });

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let dispatchers: Vec<_> = shards
            .into_iter()
            .map(|shard| tokio::spawn(dispatcher(Arc::clone(&state), shard, shutdown_rx.clone())))
            .collect();

        Ok(Self {
            state,
            shutdown_tx,
            dispatchers,
        })
    }

    /// Leave every voice channel, stop the dispatchers and let the players clean up.
    async fn shutdown(self) {
        let Self {
            state,
            shutdown_tx,
            dispatchers,
        } = self;
        info!("leaving voice channels");
        for guild_id in state.sessions.known_guilds() {
            state.sessions.teardown(guild_id, "shutdown").await;
        }

        let _ = shutdown_tx.send(true);

        for dispatcher in dispatchers {
            let _ = dispatcher.await;
        }

        state.player_tasks.close();
        // Let cancelled players disable their controls; a slow Discord request
        // must not hold shutdown indefinitely.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(12),
            state.player_tasks.wait(),
        )
        .await;
    }
}
