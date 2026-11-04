use std::{collections::HashMap, sync::Arc, time::Duration};

use songbird::{Config as SongbirdConfig, Songbird, shards::TwilightMap};
use tokio::task::JoinHandle;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::info;
use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_http::Client as HttpClient;
use twilight_model::id::{
    Id,
    marker::{ApplicationMarker, UserMarker},
};

use crate::{
    BoxError,
    audio::source::AudioContext,
    config::Config,
    discord::{connection::Discord, gateway::run_shard},
    history::PlayHistory,
    media::{
        importer::Importer,
        search_cache::SearchCache,
        spotify::SpotifyService,
        ytdlp::{YtDlpClient, YtDlpResolver},
    },
    session::SessionManager,
};

pub struct BotContext {
    pub player_tasks: TaskTracker,
    pub http: Arc<HttpClient>,
    /// Only voice states are cached, used for finding callers voice channel
    pub discord_cache: Arc<DefaultInMemoryCache>,
    pub songbird: Arc<Songbird>,
    pub sessions: Arc<SessionManager>,
    /// Turns a link into the tracks it stands for.
    pub importer: Importer,
    /// What every queued track needs to fetch its audio.
    pub audio: Arc<AudioContext>,
    /// The least time between redraws for the progress bar alone.
    pub progress_interval: Duration,
    /// All-time play stats per guild, if enabled.
    pub history: Option<Arc<PlayHistory>>,
    pub application_id: Id<ApplicationMarker>,
    pub user_id: Id<UserMarker>,
}

pub struct Bot {
    state: Arc<BotContext>,
    shutdown: CancellationToken,
    shards: Vec<JoinHandle<()>>,
}

impl Bot {
    /// Authenticate, register commands and connect every shard.
    pub async fn start(config: Config) -> Result<Self, BoxError> {
        // TLS provider needed for songbird.
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Connect to Discord
        let Discord {
            twilight_client,
            bot_application_id,
            bot_user_id,
            shards,
        } = Discord::connect(config.token).await?;
        info!("connected to Discord");

        let senders: HashMap<_, _> = shards
            .iter()
            .map(|shard| (shard.id().number(), shard.sender()))
            .collect();

        // Create the songbird instance
        let songbird = Arc::new(Songbird::twilight_from_config(
            Arc::new(TwilightMap::new(senders)),
            bot_user_id,
            SongbirdConfig::default().preallocated_tracks(2),
        ));
        info!("created songbird instance");

        // Create the play history
        let history = config
            .stats_path
            .as_deref()
            .and_then(PlayHistory::open_or_skip);
        info!(enabled = history.is_some(), "created play history");

        // Create the session manager
        let sessions =
            SessionManager::new(Arc::clone(&songbird), config.idle_timeout, history.clone());

        // Create the search cache
        let search_cache = config
            .search_cache_path
            .as_deref()
            .and_then(SearchCache::open_or_skip);
        info!(enabled = search_cache.is_some(), "created search cache");

        // Create the media clients
        let ytdlp = YtDlpClient::new(config.ytdlp);
        let resolver = YtDlpResolver::new(ytdlp.clone(), search_cache);
        info!("created YouTube client");
        let spotify_configured = config.spotify.is_some();
        let spotify = SpotifyService::new(config.spotify)?;
        info!(configured = spotify_configured, "created Spotify client");
        let importer = Importer::new(spotify, ytdlp);

        // Create the bot state
        let state = Arc::new(BotContext {
            player_tasks: TaskTracker::new(),
            http: Arc::clone(&twilight_client),
            discord_cache: Arc::new(
                DefaultInMemoryCache::builder()
                    .resource_types(ResourceType::GUILD | ResourceType::VOICE_STATE)
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
            progress_interval: config.progress_interval,
            history,
            application_id: bot_application_id,
            user_id: bot_user_id,
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

    pub async fn stop(self) {
        let Self {
            state,
            shutdown: cancellation_token,
            shards,
        } = self;
        info!("leaving voice channels");
        // Shards still deliver /play until they close below.
        state.sessions.close();
        for guild_id in state.sessions.known_guilds() {
            state.sessions.teardown(guild_id, "shutdown").await;
        }

        cancellation_token.cancel();

        for shard in shards {
            let _ = shard.await;
        }

        state.player_tasks.close();
        // Wait till player tasks are finished or timeout.
        let _ = tokio::time::timeout(Duration::from_secs(12), state.player_tasks.wait()).await;
    }
}
