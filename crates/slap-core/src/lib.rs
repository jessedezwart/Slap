//! The bot itself, independent of how it is hosted.
//!
//! A front end (the headless binary, the desktop GUI) loads a [`Config`],
//! starts a [`Bot`] on a Tokio runtime and calls [`Bot::shutdown`] when done.

mod audio;
mod config;
mod discord;
mod error;
mod media;
mod voice;

use std::{collections::HashMap, num::NonZeroUsize, sync::Arc, thread};

use serde::Serialize;
use songbird::{Config as SongbirdConfig, Songbird, shards::TwilightMap};
use tokio::{sync::watch, task::JoinHandle};
use tracing::{info, warn};
use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_gateway::{Config as GatewayConfig, Intents};
use twilight_http::Client as HttpClient;

pub use crate::{config::Config, error::ConfigError};
use crate::{
    discord::{commands, events::dispatcher, player::Panels, state::State},
    media::{
        catalog::Importer,
        search_cache::SearchResultCache,
        spotify::{SpotifyCatalog, SpotifyService},
        ytdlp::{self, PLAYLIST_LIMIT},
    },
    voice::session::{SessionManager, run_command_loop},
};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

const INTENTS: Intents = Intents::GUILDS.union(Intents::GUILD_VOICE_STATES);

/// The multi-threaded runtime the bot expects; `None` uses every core.
pub fn build_runtime(worker_threads: Option<usize>) -> std::io::Result<tokio::runtime::Runtime> {
    let worker_threads = worker_threads
        .unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZeroUsize::get));

    info!(worker_threads, "starting the tokio runtime");

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
}

/// A guild the bot is a member of, as last seen on the gateway.
#[derive(Clone, Debug, Serialize)]
pub struct GuildSummary {
    /// A string, because snowflakes do not fit in a JavaScript number.
    pub id: String,
    pub name: String,
    pub member_count: Option<u64>,
    /// Whether the bot is currently in a voice channel there.
    pub in_voice: bool,
}

/// A running bot: everything `start` built that `shutdown` takes down again.
pub struct Bot {
    state: Arc<State>,
    shutdown_tx: watch::Sender<bool>,
    dispatchers: Vec<JoinHandle<()>>,
    command_loop: JoinHandle<()>,
}

impl Bot {
    /// Authenticate, register commands and connect every shard.
    pub async fn start(config: Config) -> Result<Self, BoxError> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let config = Arc::new(config);
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

        let (sessions, commands_rx) = SessionManager::new(Arc::clone(&songbird));

        let track_cache = config.cache_path.as_deref().and_then(|path| {
            match SearchResultCache::open(path) {
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
                .resource_types(ResourceType::GUILD | ResourceType::VOICE_STATE)
                .build(),
        );

        let (ytdlp_catalog, resolver) = ytdlp::build(config.ytdlp.clone());
        let spotify = SpotifyService::new(config.spotify.clone(), PLAYLIST_LIMIT)?;
        // Spotify first: yt-dlp takes any link that is left.
        let importer = Importer::new(vec![
            Box::new(SpotifyCatalog::new(spotify)),
            Box::new(ytdlp_catalog),
        ]);

        let state = Arc::new(State {
            player_tasks: tokio_util::task::TaskTracker::new(),
            config: Arc::clone(&config),
            http: Arc::clone(&http),
            cache,
            songbird,
            sessions: Arc::clone(&sessions),
            panels: Panels::default(),
            importer,
            resolver: resolver.with_track_cache(track_cache),
            client: reqwest::Client::builder().user_agent("slap").build()?,
            application_id,
            user_id,
        });

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let command_loop = tokio::spawn(run_command_loop(Arc::clone(&sessions), commands_rx));

        let dispatchers: Vec<_> = shards
            .into_iter()
            .map(|shard| tokio::spawn(dispatcher(Arc::clone(&state), shard, shutdown_rx.clone())))
            .collect();

        Ok(Self {
            state,
            shutdown_tx,
            dispatchers,
            command_loop,
        })
    }

    /// The guilds the bot is in, sorted by name.
    pub fn guilds(&self) -> Vec<GuildSummary> {
        let mut guilds: Vec<_> = self
            .state
            .cache
            .iter()
            .guilds()
            .map(|guild| GuildSummary {
                id: guild.id().to_string(),
                name: guild.name().to_owned(),
                member_count: guild.member_count(),
                in_voice: self.state.sessions.channel_of(guild.id()).is_some(),
            })
            .collect();
        guilds.sort_by_cached_key(|guild| guild.name.to_lowercase());
        guilds
    }

    /// Leave every voice channel, stop the dispatchers and let the players clean up.
    pub async fn shutdown(self) {
        let Self {
            state,
            shutdown_tx,
            dispatchers,
            command_loop,
        } = self;
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
}
