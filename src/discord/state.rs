//! Shared handles passed to every command and event handler.

use std::sync::Arc;

use songbird::Songbird;
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_http::Client as HttpClient;
use twilight_model::id::{
    Id,
    marker::{ApplicationMarker, UserMarker},
};

use super::player::Panels;
use crate::{
    config::Config,
    media::{catalog::Importer, ytdlp::YtDlpResolver},
    voice::session::SessionManager,
};

pub struct State {
    pub player_tasks: tokio_util::task::TaskTracker,
    pub config: Arc<Config>,
    pub http: Arc<HttpClient>,
    /// Only voice states are cached, used for finding callers voice channel
    pub cache: Arc<DefaultInMemoryCache>,
    pub songbird: Arc<Songbird>,
    pub sessions: Arc<SessionManager>,
    /// Which message is each guild's player.
    pub panels: Panels,
    /// Turns a link into the tracks it stands for.
    pub importer: Importer,
    /// Turns a track into audio once the queue reaches it.
    pub resolver: YtDlpResolver,
    /// Shared connection pool for every audio fetch.
    pub client: reqwest::Client,
    pub application_id: Id<ApplicationMarker>,
    pub user_id: Id<UserMarker>,
}
