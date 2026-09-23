//! Shared handles passed to every command and event handler.

use std::sync::Arc;

use dashmap::DashMap;
use songbird::Songbird;
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_http::Client as HttpClient;
use twilight_model::id::{
    Id,
    marker::{ApplicationMarker, GuildMarker, UserMarker},
};

use crate::{
    audio::ytdlp::YtDlpService, config::Config, discord::player::Offer,
    voice::session::SessionManager,
};

pub struct State {
    pub player_tasks: tokio_util::task::TaskTracker,
    pub config: Arc<Config>,
    pub http: Arc<HttpClient>,
    /// Only voice states are cached; that is all we need to find the caller's
    /// channel, and it keeps memory flat on a small host.
    pub cache: Arc<DefaultInMemoryCache>,
    pub songbird: Arc<Songbird>,
    pub sessions: Arc<SessionManager>,
    /// The stopped player each guild may still replay, at most one per guild.
    pub replays: DashMap<Id<GuildMarker>, Offer>,
    pub ytdlp: YtDlpService,
    pub spotify: crate::spotify::SpotifyService,
    /// Shared connection pool for every audio fetch.
    pub client: reqwest::Client,
    pub application_id: Id<ApplicationMarker>,
    pub user_id: Id<UserMarker>,
}
