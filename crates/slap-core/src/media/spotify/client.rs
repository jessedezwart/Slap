//! Spotify client. Handles authorization and API requests.
//!
//! If no refresh token is provided, client credentials are used. This limits search results for playlists to the first 100 tracks.

use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::SpotifyError;
use crate::config::SpotifyConfig;

const REFRESH_TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Auth {
    refresh_token: Option<String>,
    access: Option<(String, Instant)>,
}

pub struct SpotifyClient {
    config: Option<SpotifyConfig>,
    http: Client,
    auth: Mutex<Auth>,
    pub(super) api_base: String,
    pub(super) token_url: String,
}

impl SpotifyClient {
    pub fn new(config: Option<SpotifyConfig>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            auth: Mutex::new(Auth {
                refresh_token: config.as_ref().and_then(|c| c.refresh_token.clone()),
                access: None,
            }),
            config,
            http: Client::builder()
                .timeout(Duration::from_secs(15))
                // The bearer token must never follow a redirect off the API host.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("slap")
                .build()?,
            api_base: "https://api.spotify.com/v1".into(),
            token_url: "https://accounts.spotify.com/api/token".into(),
        })
    }

    pub async fn is_authorized_as_user(&self) -> bool {
        self.auth.lock().await.refresh_token.is_some()
    }

    /// `force` discards the cached token, for when the API has just rejected it as expired.
    async fn access_token(&self, force: bool) -> Result<String, SpotifyError> {
        let config = self.config.as_ref().ok_or(SpotifyError::NotConfigured)?;
        let mut auth = self.auth.lock().await;
        if !force
            && let Some((token, expires_at)) = &auth.access
            && Instant::now() < *expires_at
        {
            return Ok(token.clone());
        }

        let form: Vec<(&str, &str)> = match auth.refresh_token.as_deref() {
            Some(token) => vec![("grant_type", "refresh_token"), ("refresh_token", token)],
            None => vec![("grant_type", "client_credentials")],
        };
        let response = self
            .http
            .post(&self.token_url)
            .basic_auth(&config.client_id, Some(&config.client_secret))
            .form(&form)
            .send()
            .await?;
        // Apart from a rate limit, any failure here means the credentials
        // were rejected.
        match response.status() {
            status if status.is_success() => {}
            StatusCode::TOO_MANY_REQUESTS => return Err(rate_limited(&response)),
            _ => return Err(SpotifyError::AuthFailed),
        }

        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            refresh_token: Option<String>,
            expires_in: Option<u64>,
        }

        let token: Token = serde_json::from_slice(&response.bytes().await?)
            .map_err(|_| SpotifyError::InvalidAuthResponse)?;
        if let Some(rotated) = token.refresh_token {
            auth.refresh_token = Some(rotated);
        }
        let lifetime = Duration::from_secs(token.expires_in.unwrap_or(3600));
        auth.access = Some((
            token.access_token.clone(),
            Instant::now() + lifetime.saturating_sub(REFRESH_TOKEN_EXPIRY_MARGIN),
        ));
        Ok(token.access_token)
    }

    /// `GET {api_base}/{path}`, retried once if the access token has expired.
    pub(super) async fn get(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, SpotifyError> {
        let endpoint = format!("{}/{path}", self.api_base);
        let mut token = self.access_token(false).await?;
        let request = |token: &str| self.http.get(&endpoint).bearer_auth(token).query(query);

        let mut response = request(&token).send().await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            token = self.access_token(true).await?;
            response = request(&token).send().await?;
        }
        let response = check_response(response)?;

        serde_json::from_slice(&response.bytes().await?)
            .map_err(|_| SpotifyError::InvalidPlaylistResponse)
    }
}

/// Map a failed API response to what it means for the playlist.
fn check_response(response: reqwest::Response) -> Result<reqwest::Response, SpotifyError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    Err(match status {
        StatusCode::TOO_MANY_REQUESTS => rate_limited(&response),
        // Without a user token Spotify reports a private playlist as missing.
        StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => SpotifyError::PlaylistNotFound,
        StatusCode::UNAUTHORIZED => SpotifyError::AuthRevoked,
        _ => SpotifyError::Status(status),
    })
}

fn rate_limited(response: &reqwest::Response) -> SpotifyError {
    SpotifyError::RateLimited(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok()),
    )
}
