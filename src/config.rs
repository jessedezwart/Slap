//! Runtime configuration, read once from the environment at start-up.

use std::{env, fmt::Display, num::ParseIntError, path::PathBuf, str::FromStr, time::Duration};

use tracing::warn;

use crate::error::ConfigError;

/// Smallest buffer we will ever allocate, regardless of configuration.
const MIN_BUFFER_BYTES: usize = 64 * 1024;
/// Upper bound, so a silly `BUFFER_MS` cannot exhaust memory across many guilds.
const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub struct Config {
    pub token: String,
    /// Tokio worker threads, or `None` for one per available core.
    pub worker_threads: Option<usize>,
    pub ytdlp: YtDlpConfig,
    pub spotify: Option<SpotifyConfig>,
    pub buffer: BufferConfig,
    /// Where the resolved-track cache lives. `CACHE_PATH=` disables it.
    pub cache_path: Option<PathBuf>,
}

/// In-memory reuse of yt-dlp resolves, shared by every guild.
///
/// This one has no off switch. A listing already resolves a single-track URL,
/// and the cache is how that resolve reaches playback; turning it off would
/// only buy a second yt-dlp run for the same track.
#[derive(Debug, Clone, Copy)]
pub struct ResolveCacheConfig {
    /// How many resolves to keep, never below [`MIN_RESOLVE_CACHE_ENTRIES`].
    pub entries: u64,
    /// Ceiling on how long one is reused. A media URL that carries its own
    /// expiry is dropped before this.
    pub max_ttl: Duration,
}

const DEFAULT_RESOLVE_CACHE_ENTRIES: u64 = 512;
const DEFAULT_RESOLVE_CACHE_TTL_MS: u64 = 600_000;

/// Floor under `RESOLVE_CACHE_ENTRIES`, low enough to be no real memory and
/// high enough that an import cannot evict its own tracks before they play.
const MIN_RESOLVE_CACHE_ENTRIES: u64 = 32;

impl Default for ResolveCacheConfig {
    fn default() -> Self {
        Self {
            entries: DEFAULT_RESOLVE_CACHE_ENTRIES,
            max_ttl: Duration::from_millis(DEFAULT_RESOLVE_CACHE_TTL_MS),
        }
    }
}

#[derive(Clone)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Optional. Client credentials can read a public playlist, but only a
    /// user token can page past its first 100 tracks.
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for SpotifyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpotifyConfig { credentials: [redacted] }")
    }
}

impl SpotifyConfig {
    fn from_env() -> Result<Option<Self>, ConfigError> {
        let read = |name| env::var(name).unwrap_or_default().trim().to_owned();
        let refresh_token = read("SPOTIFY_REFRESH_TOKEN");
        let names = ["SPOTIFY_CLIENT_ID", "SPOTIFY_CLIENT_SECRET"];
        let values = names.map(read);
        if values.iter().all(String::is_empty) && refresh_token.is_empty() {
            return Ok(None);
        }
        for (name, value) in names.iter().zip(&values) {
            if value.is_empty() {
                return Err(ConfigError::Missing(name));
            }
        }
        let [client_id, client_secret] = values;
        Ok(Some(Self {
            client_id,
            client_secret,
            refresh_token: Some(refresh_token).filter(|t| !t.is_empty()),
        }))
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
    pub cookies_file: Option<PathBuf>,
    pub resolve_cache: ResolveCacheConfig,
}

/// How much audio we hold ahead of songbird's mixer.
#[derive(Clone, Copy)]
pub struct BufferConfig {
    /// Target depth, in playback time.
    pub target: Duration,
    /// Fraction of `target` that must be buffered before playback starts.
    pub prefill_ratio: f32,
    /// Used when yt-dlp reports no bitrate. YouTube's itag 251 is ~160 kbps.
    pub assumed_bitrate_bps: u32,
}

impl BufferConfig {
    /// Playback rate in bytes per second, for turning a byte count into the
    /// playback time it represents.
    pub fn bytes_per_second(&self, bitrate_bps: Option<u32>) -> u64 {
        u64::from(bitrate_bps.unwrap_or(self.assumed_bitrate_bps).max(8_000)) / 8
    }

    /// Convert the configured playback time into a byte budget for this track.
    pub fn bytes_for(&self, bitrate_bps: Option<u32>) -> usize {
        let per_second = self.bytes_per_second(bitrate_bps);
        let millis = u64::try_from(self.target.as_millis()).unwrap_or(u64::MAX);
        let bytes = per_second * millis / 1000;

        usize::try_from(bytes)
            .unwrap_or(MAX_BUFFER_BYTES)
            .clamp(MIN_BUFFER_BYTES, MAX_BUFFER_BYTES)
    }

    /// How many bytes must arrive before we consider the track ready to play.
    pub fn prefill_bytes(&self, budget_bytes: usize) -> u64 {
        let ratio = self.prefill_ratio.clamp(0.0, 1.0);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let bytes = (budget_bytes as f32 * ratio) as u64;

        bytes.max(1)
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let token = env::var("DISCORD_TOKEN")
            .map_err(|_| ConfigError::Missing("DISCORD_TOKEN"))?
            .trim()
            .to_owned();

        if token.is_empty() {
            return Err(ConfigError::Missing("DISCORD_TOKEN"));
        }

        let cookies_file = env::var("YTDLP_COOKIES_FILE")
            .ok()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from);
        let extra_args: Vec<String> = env::var("YTDLP_EXTRA_ARGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        if cookies_file.is_none() && !extra_args.iter().any(|arg| arg.contains("cookies")) {
            warn!(
                "no YouTube cookies configured (YTDLP_COOKIES_FILE unset); \
                 yt-dlp requests may be rate-limited or rejected as bot traffic"
            );
        }

        Ok(Self {
            token,
            worker_threads: parse_worker_threads()?,
            spotify: SpotifyConfig::from_env()?,
            cache_path: match env::var("CACHE_PATH") {
                Ok(path) if path.trim().is_empty() => None,
                Ok(path) => Some(PathBuf::from(path.trim())),
                Err(_) => Some(PathBuf::from("cache.redb")),
            },
            ytdlp: YtDlpConfig {
                program: env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_owned()),
                timeout: Duration::from_millis(parse_env("YTDLP_TIMEOUT_MS", 15_000u64)?),
                extra_args,
                cookies_file,
                resolve_cache: ResolveCacheConfig {
                    // A value under the floor is raised to it rather than
                    // rejected: the cache is not something to turn off.
                    entries: parse_env("RESOLVE_CACHE_ENTRIES", DEFAULT_RESOLVE_CACHE_ENTRIES)?
                        .max(MIN_RESOLVE_CACHE_ENTRIES),
                    max_ttl: Duration::from_millis(parse_env(
                        "RESOLVE_CACHE_TTL_MS",
                        DEFAULT_RESOLVE_CACHE_TTL_MS,
                    )?),
                },
            },
            buffer: BufferConfig {
                target: Duration::from_millis(parse_env("BUFFER_MS", 5_000u64)?),
                prefill_ratio: parse_env("PREFILL_RATIO", 0.5f32)?,
                assumed_bitrate_bps: parse_env("ASSUMED_BITRATE_BPS", 160_000u32)?,
            },
        })
    }
}

/// `WORKER_THREADS`, where unset, empty, `auto` and `0` all mean "let tokio
/// size the pool from the available cores".
fn parse_worker_threads() -> Result<Option<usize>, ConfigError> {
    let Ok(raw) = env::var("WORKER_THREADS") else {
        return Ok(None);
    };

    let trimmed = raw.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }

    let count: usize = trimmed
        .parse()
        .map_err(|e: ParseIntError| ConfigError::Invalid {
            name: "WORKER_THREADS",
            value: raw.clone(),
            reason: e.to_string(),
        })?;

    Ok((count > 0).then_some(count))
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
