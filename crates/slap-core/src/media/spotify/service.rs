//! Playlist lookups, in Spotify's own terms.

use std::time::Duration;

use serde_json::Value;
use tracing::info;
use url::Url;

use super::{
    SpotifyClient, SpotifyError,
    models::{self, SpotifyPlaylist},
    playlist_id,
};
use crate::config::SpotifyConfig;

/// Max API cap
const PAGE: usize = 50;

const IMPORT_TIMEOUT: Duration = Duration::from_secs(120);

pub struct SpotifyService {
    pub(super) client: SpotifyClient,
    /// Most tracks a single import may produce.
    limit: usize,
}

impl SpotifyService {
    pub fn new(config: Option<SpotifyConfig>, limit: usize) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: SpotifyClient::new(config)?,
            limit,
        })
    }

    pub async fn playlist(&self, url: &Url) -> Result<SpotifyPlaylist, SpotifyError> {
        let id = playlist_id(url)?;
        tokio::time::timeout(IMPORT_TIMEOUT, self.playlist_by_id(id))
            .await
            .map_err(|_| SpotifyError::Timeout)?
    }

    pub(super) async fn playlist_by_id(&self, id: &str) -> Result<SpotifyPlaylist, SpotifyError> {
        // Spotify requires user authentication for `/playlists/{id}/items`, so
        // without it the only tracks we can reach are the first page embedded
        // in the playlist object itself.
        if self.client.is_authorized_as_user().await {
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
        models::playlist_name(&body)
    }

    /// Walk `/playlists/{id}/items` until the playlist or our limit runs out.
    async fn paged(&self, id: &str) -> Result<SpotifyPlaylist, SpotifyError> {
        let mut playlist = SpotifyPlaylist::default();
        let mut offset = 0;
        loop {
            let page = self
                .client
                .get(
                    &format!("playlists/{id}/items"),
                    &[("limit", PAGE.to_string()), ("offset", offset.to_string())],
                )
                .await?;
            let entries = entries(&page);
            let count = entries.len();
            self.collect_into(&mut playlist, entries);
            offset += count;

            if playlist.tracks.len() + playlist.skipped >= self.limit {
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
            playlist.name = self.name(id).await;
        }
        Ok(playlist)
    }

    /// Read the tracks embedded in the playlist object, which is all client
    /// credentials can see.
    async fn first_page_only(&self, id: &str) -> Result<SpotifyPlaylist, SpotifyError> {
        let body = self.client.get(&format!("playlists/{id}"), &[]).await?;
        let page = body.get("items").unwrap_or(&Value::Null);
        let entries = entries(page);
        let count = entries.len();

        let mut playlist = SpotifyPlaylist {
            name: models::playlist_name(&body),
            ..SpotifyPlaylist::default()
        };
        self.collect_into(&mut playlist, entries);
        playlist.truncated = total(page).is_some_and(|total| total > count) || count > self.limit;
        Ok(playlist)
    }

    /// Append entries, stopping at the limit and counting what was unplayable.
    fn collect_into(&self, playlist: &mut SpotifyPlaylist, entries: &[Value]) {
        let room = self
            .limit
            .saturating_sub(playlist.tracks.len() + playlist.skipped);
        for entry in entries.iter().take(room) {
            match models::track(entry) {
                Some(track) => playlist.tracks.push(track),
                None => playlist.skipped += 1,
            }
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
