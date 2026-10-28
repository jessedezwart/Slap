use std::time::Duration;

use serde_json::Value;
use tracing::info;
use url::Url;

use super::{Link, SpotifyClient, SpotifyError, link, parse};
use crate::{
    config::SpotifyConfig,
    media::{Import, ImportedTrack, PLAYLIST_LIMIT},
};

// Max page size allowed by the Spotify API.
const PAGE_SIZE: usize = 50;

const IMPORT_TIMEOUT: Duration = Duration::from_secs(120);

pub struct SpotifyService {
    pub(super) client: SpotifyClient,
}

impl SpotifyService {
    pub fn new(config: Option<SpotifyConfig>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: SpotifyClient::new(config)?,
        })
    }

    pub async fn import(&self, url: &Url) -> Result<Import, SpotifyError> {
        let import = async {
            match link(url)? {
                Link::Playlist(id) => self.playlist_by_id(id).await,
                Link::Album(id) => self.album_by_id(id).await,
                Link::Track(id) => self.track_by_id(id).await,
            }
        };
        tokio::time::timeout(IMPORT_TIMEOUT, import)
            .await
            .map_err(|_| SpotifyError::Timeout)?
    }

    pub(super) async fn track_by_id(&self, id: &str) -> Result<Import, SpotifyError> {
        let body = match self.client.get(&format!("tracks/{id}"), &[]).await {
            Err(SpotifyError::PlaylistNotFound | SpotifyError::PlaylistNotOwned) => {
                return Err(SpotifyError::TrackNotFound);
            }
            result => result?,
        };
        let mut import = Import::default();
        match parse::track(&body) {
            Some(track) => import.tracks.push(track),
            None => import.skipped += 1,
        }
        Ok(import)
    }

    pub(super) async fn album_by_id(&self, id: &str) -> Result<Import, SpotifyError> {
        let album = match self.client.get(&format!("albums/{id}"), &[]).await {
            Err(SpotifyError::PlaylistNotFound | SpotifyError::PlaylistNotOwned) => {
                return Err(SpotifyError::AlbumNotFound);
            }
            result => result?,
        };
        let page = album.get("tracks").unwrap_or(&Value::Null);
        let entries = entries(page);
        let count = entries.len();

        let mut import = Import {
            playlist_name: parse::playlist_name(&album),
            ..Import::default()
        };
        collect_into(&mut import, entries, |item| parse::album_track(item, &album));
        import.truncated = total(page).is_some_and(|total| total > count);
        Ok(import)
    }

    pub(super) async fn playlist_by_id(&self, id: &str) -> Result<Import, SpotifyError> {
        // Spotify requires user authentication for `/playlists/{id}/items`, so
        // without it the only tracks we can reach are the first page embedded
        // in the playlist object itself.
        if self.client.is_authorized_as_user() {
            self.paged(id).await
        } else {
            let playlist = self.first_page_only(id).await?;
            if playlist.truncated {
                info!(
                    tracks = playlist.tracks.len(),
                    "playlist truncated: paging past the first page needs a user token"
                );
            }
            Ok(playlist)
        }
    }

    /// The playlist's name, which `/items` pages do not carry.
    async fn name(&self, id: &str) -> Option<String> {
        let body = self
            .client
            .get(&format!("playlists/{id}"), &[("fields", "name".to_owned())])
            .await
            // Not worth failing the import for a name lookup failure
            .ok()?;
        parse::playlist_name(&body)
    }

    /// Walk `/playlists/{id}/items` until the playlist or our limit runs out.
    async fn paged(&self, id: &str) -> Result<Import, SpotifyError> {
        let mut playlist = Import::default();
        let mut offset = 0;
        loop {
            let page = self
                .client
                .get(
                    &format!("playlists/{id}/items"),
                    &[
                        ("limit", PAGE_SIZE.to_string()),
                        ("offset", offset.to_string()),
                    ],
                )
                .await?;
            let entries = entries(&page);
            let count = entries.len();
            collect_into(&mut playlist, entries, parse::entry);
            offset += count;

            if playlist.tracks.len() + playlist.skipped >= PLAYLIST_LIMIT {
                playlist.truncated = total(&page).is_some_and(|total| total > offset)
                    || page.get("next").is_some_and(|next| !next.is_null());
                break;
            }
            if page.get("next").is_none_or(Value::is_null) {
                break;
            }
            if count == 0 {
                return Err(SpotifyError::EmptyPage);
            }
        }
        if !playlist.tracks.is_empty() {
            playlist.playlist_name = self.name(id).await;
        }
        Ok(playlist)
    }

    /// Read the tracks embedded in the playlist object, which is all client
    /// credentials can see.
    async fn first_page_only(&self, id: &str) -> Result<Import, SpotifyError> {
        let body = self.client.get(&format!("playlists/{id}"), &[]).await?;
        // Playlists the app owner's account does not own come without `items`.
        let page = body.get("items").ok_or(SpotifyError::PlaylistNotOwned)?;
        let entries = entries(page);
        let count = entries.len();

        let mut playlist = Import {
            playlist_name: parse::playlist_name(&body),
            ..Import::default()
        };
        collect_into(&mut playlist, entries, parse::entry);
        playlist.truncated =
            total(page).is_some_and(|total| total > count) || count > PLAYLIST_LIMIT;
        Ok(playlist)
    }
}

/// Append entries, stopping at the limit and counting what was unplayable.
fn collect_into(
    playlist: &mut Import,
    entries: &[Value],
    parse: impl Fn(&Value) -> Option<ImportedTrack>,
) {
    let room = PLAYLIST_LIMIT.saturating_sub(playlist.tracks.len() + playlist.skipped);
    for entry in entries.iter().take(room) {
        match parse(entry) {
            Some(track) => playlist.tracks.push(track),
            None => playlist.skipped += 1,
        }
    }
}

fn entries(page: &Value) -> &[Value] {
    page.get("items")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn total(page: &Value) -> Option<usize> {
    page.get("total")
        .and_then(Value::as_u64)
        .and_then(|total| usize::try_from(total).ok())
}

#[cfg(test)]
#[path = "../../../tests/media/spotify/service.rs"]
mod tests;
