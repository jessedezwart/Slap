//! Spotify supplies playlist metadata; playback searches YouTube lazily.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use super::ytdlp::{ListedTrack, MediaList, PLAYLIST_LIMIT, PlaybackTarget, TrackMetadata};
use crate::{config::SpotifyConfig, error::AudioError};

pub struct Spotify {
    config: Option<SpotifyConfig>,
    client: Client,
    // Retain rotated refresh tokens for subsequent imports. Never log tokens.
    refresh_token: Mutex<Option<String>>,
    api_base: String,
    token_url: String,
}

pub fn is_spotify_url(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("open.spotify.com" | "www.open.spotify.com" | "spotify.link")
    )
}

fn playlist_id(url: &Url) -> Result<&str, AudioError> {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let parts = if parts.first().is_some_and(|p| p.starts_with("intl-")) {
        &parts[1..]
    } else {
        &parts[..]
    };
    match parts {
        ["playlist", id] if id.len() == 22 && id.bytes().all(|c| c.is_ascii_alphanumeric())
            && matches!(url.host_str(), Some("open.spotify.com" | "www.open.spotify.com")) => Ok(id),
        _ => Err(AudioError::Spotify("use a full https://open.spotify.com/playlist/... link; shortened links, albums and individual tracks are not supported".into())),
    }
}

impl Spotify {
    pub fn new(config: Option<SpotifyConfig>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            refresh_token: Mutex::new(config.as_ref().map(|c| c.refresh_token.clone())),
            config,
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("rust-music-bot")
                .build()?,
            api_base: "https://api.spotify.com/v1".into(),
            token_url: "https://accounts.spotify.com/api/token".into(),
        })
    }

    pub async fn list(&self, url: &Url) -> Result<MediaList, AudioError> {
        let id = playlist_id(url)?;
        // Bound the whole import as well as each individual HTTP request.
        tokio::time::timeout(Duration::from_secs(120), self.list_id(id))
            .await
            .map_err(|_| AudioError::Spotify("playlist import timed out; try again".into()))?
    }

    async fn access_token(&self) -> Result<String, AudioError> {
        let config = self.config.as_ref().ok_or_else(|| AudioError::Spotify(
            "playlist support needs SPOTIFY_CLIENT_ID, SPOTIFY_CLIENT_SECRET and SPOTIFY_REFRESH_TOKEN configured by the bot owner".into()
        ))?;
        let mut refresh = self.refresh_token.lock().await;
        let response = self
            .client
            .post(&self.token_url)
            .basic_auth(&config.client_id, Some(&config.client_secret))
            .form(&[
                ("grant_type", "refresh_token"),
                (
                    "refresh_token",
                    refresh.as_deref().unwrap_or(&config.refresh_token),
                ),
            ])
            .send()
            .await?;
        let response = check_response(response, true)?;
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            refresh_token: Option<String>,
        }
        let token: Token = serde_json::from_slice(&response.bytes().await?)
            .map_err(|_| AudioError::Spotify("invalid authorization response".into()))?;
        if let Some(rotated) = token.refresh_token {
            *refresh = Some(rotated);
        }
        Ok(token.access_token)
    }

    async fn list_id(&self, id: &str) -> Result<MediaList, AudioError> {
        let mut token = self.access_token().await?;
        let mut listing = MediaList {
            tracks: Vec::new(),
            skipped: 0,
            truncated: false,
            single: None,
        };
        let mut offset = 0;
        loop {
            let endpoint = format!("{}/playlists/{id}/items", self.api_base);
            let get = |token: &str| {
                self.client
                    .get(&endpoint)
                    .bearer_auth(token)
                    .query(&[("limit", 50), ("offset", offset)])
            };
            let mut response = get(&token).send().await?;
            if response.status() == StatusCode::UNAUTHORIZED {
                token = self.access_token().await?;
                response = get(&token).send().await?;
            }
            let response = check_response(response, false)?;
            let page: Page = serde_json::from_slice(&response.bytes().await?)
                .map_err(|_| AudioError::Spotify("invalid playlist response".into()))?;
            let count = page.items.len();
            for item in page.items.iter().take(PLAYLIST_LIMIT - offset) {
                if let Some(track) = listed_track(item) {
                    listing.tracks.push(track);
                } else {
                    listing.skipped += 1;
                }
            }
            offset += count;
            if offset >= PLAYLIST_LIMIT {
                listing.truncated = offset > PLAYLIST_LIMIT || page.next.is_some();
                break;
            }
            if page.next.is_none() {
                break;
            }
            if count == 0 {
                return Err(AudioError::Spotify(
                    "playlist pagination returned an empty page; try again".into(),
                ));
            }
            // Build the next URL ourselves: credentials never follow a URL from the response.
        }
        Ok(listing)
    }
}

#[derive(Deserialize)]
struct Page {
    items: Vec<Value>,
    next: Option<String>,
}

fn check_response(
    response: reqwest::Response,
    auth: bool,
) -> Result<reqwest::Response, AudioError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let message = match status {
        StatusCode::TOO_MANY_REQUESTS => {
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            return Err(AudioError::Spotify(match seconds {
                Some(seconds) => format!("rate limit reached; try again in {seconds} seconds"),
                None => "rate limit reached; try again later".into(),
            }));
        }
        _ if auth => "authorization failed; check the Spotify client credentials and refresh token",
        StatusCode::UNAUTHORIZED => {
            "authorization expired or was revoked; reauthorize the Spotify account"
        }
        StatusCode::FORBIDDEN => {
            "playlist access denied; the authorized account must own or collaborate on the playlist and have playlist-read-private permission"
        }
        StatusCode::NOT_FOUND => {
            "playlist not found or inaccessible to the authorized Spotify account"
        }
        _ => {
            return Err(AudioError::Spotify(format!(
                "API returned HTTP {status}; try again later"
            )));
        }
    };
    Err(AudioError::Spotify(message.into()))
}

fn listed_track(entry: &Value) -> Option<ListedTrack> {
    // `item` is the current API field; older responses used `track`.
    let track = entry.get("item").or_else(|| entry.get("track"))?;
    if entry.get("is_local").and_then(Value::as_bool) == Some(true)
        || track.get("is_local").and_then(Value::as_bool) == Some(true)
        || track.get("type").and_then(Value::as_str) != Some("track")
    {
        return None;
    }
    let title = track.get("name")?.as_str()?.trim();
    let artists: Vec<_> = track
        .get("artists")?
        .as_array()?
        .iter()
        .filter_map(|a| a.get("name")?.as_str())
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect();
    if title.is_empty() || artists.is_empty() {
        return None;
    }
    let artist = artists.join(", ");
    Some(ListedTrack {
        target: PlaybackTarget::YoutubeSearch(format!("{artist} {title}")),
        metadata: TrackMetadata {
            title: Some(format!("{artist} — {title}")),
            uploader: Some(artist),
            duration: track
                .get("duration_ms")
                .and_then(Value::as_u64)
                .map(Duration::from_millis),
            webpage_url: track
                .pointer("/external_urls/spotify")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..TrackMetadata::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn song(name: &str) -> Value {
        json!({"item": {"type":"track", "name":name,
            "artists":[{"name":"Artist"},{"name":"Guest"}], "duration_ms":123456,
            "external_urls":{"spotify":"https://open.spotify.com/track/example"}}})
    }

    #[test]
    fn playlist_links_and_unsupported_spotify_links() {
        for path in [
            "playlist/1234567890123456789012?si=abc",
            "intl-nl/playlist/1234567890123456789012/",
        ] {
            assert_eq!(
                playlist_id(&Url::parse(&format!("https://open.spotify.com/{path}")).unwrap())
                    .unwrap(),
                "1234567890123456789012"
            );
        }
        for raw in [
            "https://spotify.link/abc",
            "https://open.spotify.com/album/1234567890123456789012",
            "https://open.spotify.com/playlist/invalid",
            "https://example.com/playlist/1234567890123456789012",
        ] {
            assert!(playlist_id(&Url::parse(raw).unwrap()).is_err());
        }
        assert!(!is_spotify_url(
            &Url::parse("https://open.spotify.com.evil.test/playlist/x").unwrap()
        ));
    }

    #[test]
    fn metadata_becomes_artist_and_title_search() {
        let listed = listed_track(&song("Song & punctuation?!")).unwrap();
        assert!(
            matches!(listed.target, PlaybackTarget::YoutubeSearch(ref q) if q == "Artist, Guest Song & punctuation?!")
        );
        assert_eq!(
            listed.metadata.duration,
            Some(Duration::from_millis(123456))
        );
        let mut old = song("Legacy");
        old["track"] = old.as_object_mut().unwrap().remove("item").unwrap();
        assert!(listed_track(&old).is_some());
        for entry in [
            Value::Null,
            json!({"item":null}),
            json!({"item":{"type":"episode"}}),
            json!({"item":{"type":"track", "name":"", "artists":[]}}),
        ] {
            assert!(listed_track(&entry).is_none());
        }
        let mut local = song("Local");
        local["is_local"] = json!(true);
        assert!(listed_track(&local).is_none());
    }

    // A local HTTP server verifies real auth, pagination, limits and error handling.
    async fn server(
        responses: Vec<(u16, Value)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length: ")?.parse::<usize>().ok()
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        (base, task)
    }

    fn client(base: &str) -> Spotify {
        let mut spotify = Spotify::new(Some(SpotifyConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
            refresh_token: "refresh".into(),
        }))
        .unwrap();
        spotify.api_base = base.into();
        spotify.token_url = format!("{base}/token");
        spotify
    }

    #[tokio::test]
    async fn imports_pages_in_order_skips_unavailable_and_refreshes_authorization() {
        let (base, task) = server(vec![
            (200, json!({"access_token":"first", "refresh_token":"rotated"})),
            (200, json!({"items":[song("First"), null], "next":"https://untrusted.invalid/never-follow"})),
            (401, json!({})),
            (200, json!({"access_token":"second"})),
            (200, json!({"items":[song("Last")], "next":null})),
        ]).await;
        let list = client(&base).list_id("playlist").await.unwrap();
        assert_eq!(list.tracks.len(), 2);
        assert_eq!(list.skipped, 1);
        assert!(!list.truncated);
        assert_eq!(
            list.tracks[1].metadata.title.as_deref(),
            Some("Artist, Guest — Last")
        );
        let requests = task.await.unwrap();
        assert!(requests[0].contains("grant_type=refresh_token&refresh_token=refresh"));
        assert!(requests[2].starts_with("GET /playlists/playlist/items?limit=50&offset=2 "));
        assert!(requests[3].contains("refresh_token=rotated"));
        assert!(
            requests[4]
                .to_ascii_lowercase()
                .contains("authorization: bearer second")
        );
    }

    #[tokio::test]
    async fn import_stops_at_limit_and_reports_truncation() {
        let mut responses = vec![(200, json!({"access_token":"token"}))];
        for _ in 0..10 {
            responses.push((
                200,
                json!({"items":vec![song("Repeated"); 50], "next":"more"}),
            ));
        }
        let (base, task) = server(responses).await;
        let list = client(&base).list_id("playlist").await.unwrap();
        assert_eq!(list.tracks.len(), PLAYLIST_LIMIT);
        assert!(list.truncated);
        assert_eq!(task.await.unwrap().len(), 11);
    }

    #[tokio::test]
    async fn actionable_errors_and_empty_playlist() {
        let disabled = Spotify::new(None).unwrap();
        assert!(
            disabled
                .list_id("unused")
                .await
                .unwrap_err()
                .to_string()
                .contains("SPOTIFY_CLIENT_ID")
        );
        for (status, expected) in [
            (403, "own or collaborate"),
            (404, "not found"),
            (429, "rate limit"),
        ] {
            let (base, task) = server(vec![
                (200, json!({"access_token":"token"})),
                (status, json!({})),
            ])
            .await;
            assert!(
                client(&base)
                    .list_id("playlist")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
            task.await.unwrap();
        }
        let (base, task) = server(vec![
            (200, json!({"access_token":"token"})),
            (200, json!({"items":[], "next":null})),
        ])
        .await;
        assert!(
            client(&base)
                .list_id("playlist")
                .await
                .unwrap()
                .tracks
                .is_empty()
        );
        task.await.unwrap();
    }
}
