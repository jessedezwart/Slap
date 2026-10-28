use tracing::debug;
use url::Url;

use super::{
    Import, Input, MediaError,
    spotify::{SpotifyService, is_spotify_url},
    ytdlp::YtDlpClient,
};

/// Hands each link to the source that understands it, and searches to yt-dlp.
pub struct Importer {
    spotify: SpotifyService,
    ytdlp: YtDlpClient,
}

impl Importer {
    pub fn new(spotify: SpotifyService, ytdlp: YtDlpClient) -> Self {
        Self { spotify, ytdlp }
    }

    pub async fn import(&self, input: &Input) -> Result<Import, MediaError> {
        match input {
            Input::Link(url) => self.import_link(url).await,
            Input::Search(text) => {
                debug!(source = "search", "importing search");
                self.ytdlp.search(text).await
            }
        }
    }

    async fn import_link(&self, url: &Url) -> Result<Import, MediaError> {
        if is_spotify_url(url) {
            debug!(source = "Spotify", "importing link");
            return Ok(self.spotify.import(url).await?);
        }

        // yt-dlp supports far more sites than can be listed, so give it every
        // other link and let yt-dlp decide.
        debug!(source = "yt-dlp", host = url.host_str(), "importing link");
        Ok(self.ytdlp.import(url).await?)
    }
}
