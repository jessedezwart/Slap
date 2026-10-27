//! Shared handles passed to every command and event handler.

use std::sync::Arc;

use songbird::Songbird;
use tokio_util::task::TaskTracker;
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_http::Client as HttpClient;
use twilight_model::id::{
    Id,
    marker::{ApplicationMarker, UserMarker},
};

use crate::{audio::source::AudioContext, media::importer::Importer, session::SessionManager};

pub struct State {
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
    pub application_id: Id<ApplicationMarker>,
    pub user_id: Id<UserMarker>,
}
