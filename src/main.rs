mod audio;
mod config;
mod discord;
mod error;
mod spotify;
mod voice;

use std::{collections::HashMap, num::NonZeroUsize, process::ExitCode, sync::Arc, thread};

use songbird::{Config as SongbirdConfig, Songbird, shards::TwilightMap};
use tokio::{signal, sync::watch, task::JoinHandle};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_gateway::{Config as GatewayConfig, Intents};
use twilight_http::Client as HttpClient;

use crate::{
    audio::ytdlp::YtDlpService,
    config::Config,
    discord::{commands, events::dispatcher, state::State},
    voice::session::{SessionManager, run_command_loop},
};

const INTENTS: Intents = Intents::GUILDS.union(Intents::GUILD_VOICE_STATES);

fn main() -> ExitCode {
    let dotenv = dotenvy::dotenv();

    // A slow terminal/container log sink must not block the audio driver or
    // Tokio workers. Bound memory and drop excess logs under sustained pressure.
    // Keep the guard alive through runtime shutdown to flush remaining lines.
    let (log_writer, _log_guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(1024)
        .lossy(true)
        .thread_name("log-writer")
        .finish(std::io::stdout());
    tracing_subscriber::fmt()
        .with_writer(log_writer)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("rust_music_bot=info,warn")),
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

    let worker_threads = config
        .worker_threads
        .unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZeroUsize::get));

    info!(worker_threads, "starting the tokio runtime");

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
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

/// Everything `startup` built that `shutdown` needs to take down again.
struct Running {
    state: Arc<State>,
    shutdown_tx: watch::Sender<bool>,
    dispatchers: Vec<JoinHandle<()>>,
    command_loop: JoinHandle<()>,
}

async fn run(config: Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let running = startup(config).await?;
    info!("ready");

    signal::ctrl_c().await?;
    info!("shutting down");

    shutdown(running).await;

    Ok(())
}

async fn startup(config: Config) -> Result<Running, Box<dyn std::error::Error + Send + Sync>> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = Arc::new(config);
    let http = Arc::new(HttpClient::new(config.token.clone()));

    let application_id = http.current_user_application().await?.model().await?.id;
    let user_id = http.current_user().await?.model().await?.id;

    info!(%application_id, %user_id, "authenticated");

    http.interaction(application_id)
        .set_global_commands(&commands::all_commands())
        .await?;

    let gateway_config = GatewayConfig::new(config.token.clone(), INTENTS);
    let shards: Vec<_> =
        twilight_gateway::create_recommended(&http, gateway_config, |_, builder| builder.build())
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

    let (sessions, commands_rx) = SessionManager::new(Arc::clone(&songbird));

    let track_cache = config.cache_path.as_deref().and_then(|path| {
        match crate::audio::cache::SpotifyTrackCache::open(path) {
            Ok(cache) => {
                info!(path = %path.display(), "track cache opened");
                Some(Arc::new(cache))
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "could not open track cache; continuing without it");
                None
            }
        }
    });

    let cache = Arc::new(
        DefaultInMemoryCache::builder()
            .resource_types(ResourceType::VOICE_STATE)
            .build(),
    );

    let state = Arc::new(State {
        player_tasks: tokio_util::task::TaskTracker::new(),
        config: Arc::clone(&config),
        http: Arc::clone(&http),
        cache,
        songbird,
        sessions: Arc::clone(&sessions),
        ytdlp: YtDlpService::new(config.ytdlp.clone()).with_track_cache(track_cache),
        spotify: crate::spotify::SpotifyService::new(
            config.spotify.clone(),
            crate::audio::ytdlp::PLAYLIST_LIMIT,
        )?,
        client: reqwest::Client::builder()
            .user_agent("rust-music-bot")
            .build()?,
        application_id,
        user_id,
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let command_loop = tokio::spawn(run_command_loop(Arc::clone(&sessions), commands_rx));

    let dispatchers: Vec<_> = shards
        .into_iter()
        .map(|shard| tokio::spawn(dispatcher(Arc::clone(&state), shard, shutdown_rx.clone())))
        .collect();

    Ok(Running {
        state,
        shutdown_tx,
        dispatchers,
        command_loop,
    })
}

/// Leave every voice channel, stop the dispatchers and let the players clean up.
async fn shutdown(running: Running) {
    let Running {
        state,
        shutdown_tx,
        dispatchers,
        command_loop,
    } = running;
    info!("leaving voice channels");
    for guild_id in state.sessions.active_guilds() {
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

    command_loop.abort();
}
