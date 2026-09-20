//! Authorized HTTP access to the Web API.
//!
//! Two grants are supported. When the bot owner has supplied a refresh token
//! the client acts as that user, which is the only way to page past a
//! playlist's first 100 tracks. Otherwise it falls back to client credentials,
//! which can still read public playlists.

use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::SpotifyError;
use crate::config::SpotifyConfig;

/// Renew this long before a token actually expires, so a request cannot go out
/// with one that dies in flight.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// Never log any field of this. The mutex also makes concurrent imports share
/// one token fetch rather than racing to refresh.
#[derive(Default)]
struct Auth {
    /// Retained across imports because Spotify may rotate it on each refresh.
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
                .user_agent("rust-music-bot")
                .build()?,
            api_base: "https://api.spotify.com/v1".into(),
            token_url: "https://accounts.spotify.com/api/token".into(),
        })
    }

    /// Whether requests are made as a user rather than as the application.
    ///
    /// Spotify requires user authentication for the playlist item endpoints,
    /// so this decides which import strategy the service can use.
    pub async fn is_authorized_as_user(&self) -> bool {
        self.auth.lock().await.refresh_token.is_some()
    }

    /// A usable access token, reusing the current one until it nears expiry.
    ///
    /// `force` discards the cached token, for when the API has just rejected
    /// it as expired.
    async fn access_token(&self, force: bool) -> Result<String, SpotifyError> {
        let config = self.config.as_ref().ok_or_else(|| SpotifyError::api(
            "playlist support needs SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET configured by the bot owner",
        ))?;
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
        let response = check_response(response, true)?;

        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            refresh_token: Option<String>,
            expires_in: Option<u64>,
        }

        let token: Token = serde_json::from_slice(&response.bytes().await?)
            .map_err(|_| SpotifyError::api("invalid authorization response"))?;
        if let Some(rotated) = token.refresh_token {
            auth.refresh_token = Some(rotated);
        }
        let lifetime = Duration::from_secs(token.expires_in.unwrap_or(3600));
        auth.access = Some((
            token.access_token.clone(),
            Instant::now() + lifetime.saturating_sub(EXPIRY_MARGIN),
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
        let response = check_response(response, false)?;

        serde_json::from_slice(&response.bytes().await?)
            .map_err(|_| SpotifyError::api("invalid playlist response"))
    }
}

fn check_response(
    response: reqwest::Response,
    auth: bool,
) -> Result<reqwest::Response, SpotifyError> {
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
            return Err(SpotifyError::Api(match seconds {
                Some(seconds) => format!("rate limit reached; try again in {seconds} seconds"),
                None => "rate limit reached; try again later".into(),
            }));
        }
        _ if auth => "authorization failed; check the Spotify client credentials",
        // Without a user token Spotify reports a private playlist as missing,
        // so both statuses get the same actionable advice.
        StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => {
            "playlist not found — make sure it is set to public, and that the link is a full https://open.spotify.com/playlist/... link"
        }
        StatusCode::UNAUTHORIZED => "authorization expired or was revoked; reauthorize the bot",
        _ => {
            return Err(SpotifyError::Api(format!(
                "API returned HTTP {status}; try again later"
            )));
        }
    };
    Err(SpotifyError::api(message))
}
