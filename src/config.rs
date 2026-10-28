use std::{env, fmt::Display, path::PathBuf, str::FromStr, time::Duration};

use thiserror::Error;
use tracing::warn;

/// Failures while reading configuration out of the environment.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("required environment variable `{0}` is not set")]
    Missing(&'static str),

    #[error("environment variable `{name}` has an invalid value `{value}`: {reason}")]
    Invalid {
        name: &'static str,
        value: String,
        reason: String,
    },
}

pub struct Config {
    pub token: String,
    pub ytdlp: YtDlpConfig,
    pub spotify: Option<SpotifyConfig>,
    /// How much audio we hold ahead of songbird's mixer, in playback time.
    pub buffer: Duration,
    /// Leave the voice channel after nothing has played for this long.
    pub idle_timeout: Duration,
    /// Where the search cache lives. `CACHE_PATH=` disables it.
    pub search_cache_path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Optional. Client credentials can read a public playlist, but only a
    /// user token can page past its first 100 tracks.
    pub refresh_token: Option<String>,
}

impl SpotifyConfig {
    fn from_env() -> Result<Option<Self>, ConfigError> {
        let refresh_token = env_string("SPOTIFY_REFRESH_TOKEN");
        match (
            env_string("SPOTIFY_CLIENT_ID"),
            env_string("SPOTIFY_CLIENT_SECRET"),
        ) {
            (None, None) if refresh_token.is_none() => Ok(None),
            (Some(client_id), Some(client_secret)) => Ok(Some(Self {
                client_id,
                client_secret,
                refresh_token,
            })),
            (None, _) => Err(ConfigError::Missing("SPOTIFY_CLIENT_ID")),
            (_, None) => Err(ConfigError::Missing("SPOTIFY_CLIENT_SECRET")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct YtDlpConfig {
    /// Binary to spawn. Always invoked as argv, never through a shell.
    pub program: String,
    pub timeout: Duration,
    /// Extra argv entries, e.g. `--cookies-from-browser firefox`. Split on
    /// whitespace; still argv, still no shell.
    pub extra_args: Vec<String>,
    pub cookies_file: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let token = env_string("DISCORD_TOKEN").ok_or(ConfigError::Missing("DISCORD_TOKEN"))?;

        let cookies_file = env_string("YTDLP_COOKIES_FILE")
            .map_or_else(|| PathBuf::from("secrets/cookies.txt"), PathBuf::from);
        let extra_args: Vec<String> = env::var("YTDLP_EXTRA_ARGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        if !cookies_file.exists() && !extra_args.iter().any(|arg| arg.contains("cookies")) {
            warn!(
                path = %cookies_file.display(),
                "no YouTube cookies file found; \
                 yt-dlp requests may be rate-limited or rejected as bot traffic"
            );
        }

        Ok(Self {
            token,
            spotify: SpotifyConfig::from_env()?,
            // Unlike the other settings, set-but-empty means "off" here.
            search_cache_path: match env::var("CACHE_PATH") {
                Ok(path) if path.trim().is_empty() => None,
                Ok(path) => Some(PathBuf::from(path.trim())),
                Err(_) => Some(PathBuf::from("cache.redb")),
            },
            ytdlp: YtDlpConfig {
                program: env_string("YTDLP_PATH").unwrap_or_else(|| "yt-dlp".to_owned()),
                timeout: Duration::from_millis(parse_env("YTDLP_TIMEOUT_MS", 15_000)?),
                extra_args,
                cookies_file,
            },
            buffer: Duration::from_millis(parse_env("BUFFER_MS", 2_000)?),
            idle_timeout: Duration::from_secs(parse_env("IDLE_TIMEOUT_SECS", 300)?),
        })
    }
}

/// A variable's trimmed value; `None` when it is unset or blank.
fn env_string(name: &str) -> Option<String> {
    let value = env::var(name).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_env<T>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: Display,
{
    match env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|e: T::Err| ConfigError::Invalid {
                name,
                value: raw,
                reason: e.to_string(),
            }),
    }
}
