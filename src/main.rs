//! A Discord music bot that streams YouTube's Opus audio straight through to
//! Discord: yt-dlp resolves the link, we fetch the WebM/Opus bytes ourselves,
//! and songbird passes the Opus packets to the voice socket without ever
//! decoding or re-encoding them.

mod audio;
mod config;
mod discord;
mod error;
mod voice;

use std::{collections::HashMap, num::NonZeroUsize, process::ExitCode, sync::Arc, thread};

use songbird::{Config as SongbirdConfig, Songbird, shards::TwilightMap};
use tokio::{signal, sync::watch};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_gateway::{Config as GatewayConfig, Intents};
use twilight_http::Client as HttpClient;

use crate::{
    audio::ytdlp::YtDlp,
    config::Config,
    discord::{commands, events::dispatcher, state::State},
    voice::session::{SessionManager, run_command_loop},
};

/// We only need to see guilds and who is in which voice channel. Nothing here
/// is privileged, so the bot works without any gateway intent approval.
const INTENTS: Intents = Intents::GUILDS.union(Intents::GUILD_VOICE_STATES);

fn main() -> ExitCode {
    // Before the subscriber is built, so a `RUST_LOG` set in `.env` still
    // applies. Existing environment variables win over the file, which keeps
    // `DISCORD_TOKEN=... cargo run` working with a stale `.env` on disk.
    let dotenv = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("rust_music_bot=info,warn")),
        )
        .init();

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

async fn run(config: Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Installed explicitly so the choice of provider does not depend on which
    // crate in the graph happened to enable which feature.
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
        // Two tracks: the one playing plus the one the queue preloads ahead of
        // it. Beyond that songbird reallocates, which we would rather avoid on
        // the audio thread.
        SongbirdConfig::default().preallocated_tracks(2),
    ));

    let (sessions, commands_rx) = SessionManager::new(Arc::clone(&songbird));

    let cache = Arc::new(
        DefaultInMemoryCache::builder()
            .resource_types(ResourceType::VOICE_STATE)
            .build(),
    );

    let state = Arc::new(State {
        config: Arc::clone(&config),
        http: Arc::clone(&http),
        cache,
        songbird,
        sessions: Arc::clone(&sessions),
        ytdlp: YtDlp::new(config.ytdlp.clone()),
        spotify: crate::audio::spotify::Spotify::new(config.spotify.clone())?,
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

    signal::ctrl_c().await?;
    info!("shutting down");

    for guild_id in state.sessions.active_guilds() {
        state.sessions.teardown(guild_id, "shutdown").await;
    }

    let _ = shutdown_tx.send(true);

    for dispatcher in dispatchers {
        let _ = dispatcher.await;
    }

    command_loop.abort();

    Ok(())
}
