use url::Url;

use super::{SpotifyPlaylist, SpotifyService, SpotifyTrack, is_spotify_url};
use crate::media::{
    Import, MediaError, Metadata, SearchQuery, Target, Track, catalog::Catalog, ytdlp::http_url,
};

pub struct SpotifyCatalog {
    service: SpotifyService,
}

impl SpotifyCatalog {
    pub fn new(service: SpotifyService) -> Self {
        Self { service }
    }
}

#[async_trait::async_trait]
impl Catalog for SpotifyCatalog {
    fn name(&self) -> &'static str {
        "Spotify"
    }

    fn handles(&self, url: &Url) -> bool {
        is_spotify_url(url)
    }

    async fn list(&self, url: &Url) -> Result<Import, MediaError> {
        Ok(self.service.playlist(url).await?.into())
    }
}

impl From<SpotifyPlaylist> for Import {
    fn from(playlist: SpotifyPlaylist) -> Self {
        Self {
            tracks: playlist.tracks.into_iter().map(Track::from).collect(),
            skipped: playlist.skipped,
            truncated: playlist.truncated,
            playlist_name: playlist.name,
        }
    }
}

impl From<SpotifyTrack> for Track {
    fn from(track: SpotifyTrack) -> Self {
        let artist = track.artist();
        Self {
            metadata: Metadata {
                title: Some(format!("{artist} — {}", track.title)),
                uploader: Some(artist.clone()),
                duration: track.duration,
                thumbnail: track.album_art,
                webpage_url: track.url.as_deref().and_then(http_url),
            },
            target: Target::Search(SearchQuery {
                key: track.id,
                artist,
                title: track.title,
                duration: track.duration,
            }),
            // Spotify has no audio
            stream: None,
        }
    }
}
