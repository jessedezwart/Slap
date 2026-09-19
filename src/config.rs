//! Runtime configuration, read once from the environment at start-up.

use std::{env, fmt::Display, str::FromStr, time::Duration};

use crate::error::ConfigError;

/// Smallest buffer we will ever allocate, regardless of configuration.
const MIN_BUFFER_BYTES: usize = 64 * 1024;
/// Upper bound, so a silly `BUFFER_MS` cannot exhaust memory across many guilds.
const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    /// Tokio worker threads. One is the default: the realtime work happens on
    /// songbird's own mixer thread, not here.
    pub worker_threads: usize,
    pub ytdlp: YtDlpConfig,
    pub buffer: BufferConfig,
}

#[derive(Debug, Clone)]
pub struct YtDlpConfig {
    /// Binary to spawn. Always invoked as argv, never through a shell.
    pub program: String,
    pub timeout: Duration,
    /// Extra argv entries, e.g. `--cookies-from-browser firefox`. Split on
    /// whitespace; still argv, still no shell.
    pub extra_args: Vec<String>,
}

/// How much audio we hold ahead of songbird's mixer.
#[derive(Debug, Clone, Copy)]
pub struct BufferConfig {
    /// Target depth, in playback time.
    pub target: Duration,
    /// Fraction of `target` that must be buffered before playback starts.
    pub prefill_ratio: f32,
    /// How long to wait for that prefill before starting anyway.
    pub prefill_timeout: Duration,
    /// Used when yt-dlp reports no bitrate. YouTube's itag 251 is ~160 kbps.
    pub assumed_bitrate_bps: u32,
}

impl BufferConfig {
    /// Convert the configured playback time into a byte budget for this track.
    pub fn bytes_for(&self, bitrate_bps: Option<u32>) -> usize {
        let bps = u64::from(bitrate_bps.unwrap_or(self.assumed_bitrate_bps).max(8_000));
        let millis = u64::try_from(self.target.as_millis()).unwrap_or(u64::MAX);
        let bytes = bps / 8 * millis / 1000;

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

        Ok(Self {
            token,
            worker_threads: parse_env("WORKER_THREADS", 1usize)?.max(1),
            ytdlp: YtDlpConfig {
                program: env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_owned()),
                timeout: Duration::from_millis(parse_env("YTDLP_TIMEOUT_MS", 15_000u64)?),
                extra_args: env::var("YTDLP_EXTRA_ARGS")
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
            },
            buffer: BufferConfig {
                target: Duration::from_millis(parse_env("BUFFER_MS", 5_000u64)?),
                prefill_ratio: parse_env("PREFILL_RATIO", 0.5f32)?,
                prefill_timeout: Duration::from_millis(parse_env("PREFILL_TIMEOUT_MS", 10_000u64)?),
                assumed_bitrate_bps: parse_env("ASSUMED_BITRATE_BPS", 160_000u32)?,
            },
        })
    }
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
