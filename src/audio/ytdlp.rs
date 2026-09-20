//! yt-dlp invocation.
//!
//! yt-dlp is only ever used to *resolve* a video into a direct media URL plus
//! the headers needed to fetch it; the bytes themselves are pulled by
//! [`crate::audio::stream`]. The process is spawned as argv — there is no shell
//! anywhere in this path.

use std::{collections::BTreeMap, process::Stdio, sync::Arc, time::Duration, time::Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};
use serde::Deserialize;
use tokio::{process::Command, time};
use tracing::{debug, info, warn};
use twilight_model::id::{Id, marker::UserMarker};
use url::Url;

use super::{cache::TrackCache, matching};
use crate::{config::YtDlpConfig, error::AudioError, spotify::SpotifyTrack};

/// Opus-only format selection.
///
/// Restricting to `acodec=opus` guarantees whatever yt-dlp picks needs no
/// transcoding, while still letting it pick the highest-bitrate Opus
/// rendition available for the video (rather than pinning to a fixed itag).
const OPUS_FORMAT: &str = "bestaudio[acodec=opus]";

/// Keeps a console window from flashing up per invocation on Windows.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Everything needed to open the audio stream, plus what we show the user.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub media_url: String,
    pub headers: HeaderMap,
    pub content_length: Option<u64>,
    pub bitrate_bps: Option<u32>,
    pub metadata: TrackMetadata,
}

#[derive(Debug, Clone, Default)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub duration: Option<Duration>,
    pub uploader: Option<String>,
    pub thumbnail: Option<String>,
    pub webpage_url: Option<String>,
    /// Who queued the track; shown on the player.
    pub requester: Option<Id<UserMarker>>,
    /// Enqueue order, so turning shuffle off can restore it.
    pub sequence: u64,
}

#[derive(Clone)]
pub struct YtDlp {
    config: YtDlpConfig,
    /// Skips the search for tracks resolved on an earlier run.
    cache: Option<Arc<TrackCache>>,
}

/// Limit imports so accidental channel/radio links cannot create an unbounded queue.
pub const PLAYLIST_LIMIT: usize = 1000;

/// Searches are kept lazy, just like direct video URLs.
#[derive(Debug)]
pub enum PlaybackTarget {
    Url(Url),
    /// A Spotify track to go and find an upload of.
    Search(SpotifyTrack),
}

/// Where searches look, best source first.
#[derive(Clone, Copy)]
enum SearchSource {
    /// Label releases and official audio, without video intros.
    YoutubeMusic,
    /// Everything else: fan uploads, lyric videos, music videos.
    Youtube,
}

impl SearchSource {
    const ORDER: [Self; 2] = [Self::YoutubeMusic, Self::Youtube];

    fn name(self) -> &'static str {
        match self {
            Self::YoutubeMusic => "YouTube Music",
            Self::Youtube => "YouTube",
        }
    }

    /// The yt-dlp input; still one argv entry, so the text stays literal.
    fn input(self, text: &str) -> String {
        match self {
            Self::YoutubeMusic => {
                let mut url =
                    Url::parse_with_params("https://music.youtube.com/search", [("q", text)])
                        .expect("static search URL is valid");
                // Songs only: videos are what the YouTube fallback is for.
                url.set_fragment(Some("songs"));
                url.into()
            }
            Self::Youtube => format!("ytsearch1:{text}"),
        }
    }
}

impl From<Url> for PlaybackTarget {
    fn from(url: Url) -> Self {
        Self::Url(url)
    }
}

#[derive(Debug)]
pub struct ListedTrack {
    pub target: PlaybackTarget,
    pub metadata: TrackMetadata,
}

#[derive(Debug)]
pub struct MediaList {
    pub tracks: Vec<ListedTrack>,
    pub skipped: usize,
    pub truncated: bool,
    pub single: Option<Resolved>,
}

impl YtDlp {
    pub fn new(config: YtDlpConfig) -> Self {
        Self {
            config,
            cache: None,
        }
    }

    pub fn with_cache(mut self, cache: Option<Arc<TrackCache>>) -> Self {
        self.cache = cache;
        self
    }

    /// Check that `raw` is an HTTP(S) URL to hand to yt-dlp.
    pub fn validate_url(raw: &str) -> Result<Url, AudioError> {
        let url = Url::parse(raw.trim()).map_err(|_| AudioError::UnsupportedUrl(raw.to_owned()))?;

        let scheme_ok = matches!(url.scheme(), "http" | "https");
        if scheme_ok && url.host_str().is_some() {
            Ok(url)
        } else {
            Err(AudioError::UnsupportedUrl(raw.to_owned()))
        }
    }

    /// Resolve a link, or find and resolve the recording a search asks for.
    pub async fn resolve(&self, target: &PlaybackTarget) -> Result<Resolved, AudioError> {
        match target {
            PlaybackTarget::Url(url) => {
                info!(host = url.host_str(), "resolving audio source");
                self.resolve_input(url.as_str()).await
            }
            PlaybackTarget::Search(wanted) => self.resolve_search(wanted).await,
        }
    }

    /// Resolve each source's top result until one runs about as long as the
    /// requested track.
    async fn resolve_search(&self, wanted: &SpotifyTrack) -> Result<Resolved, AudioError> {
        if let Some(url) = self.cache.as_ref().and_then(|c| c.get(&wanted.id)) {
            info!(spotify_id = wanted.id, "reusing previously resolved upload");
            match self.resolve_input(&url).await {
                Ok(resolved) => return Ok(resolved),
                // The upload can have been taken down since; search again.
                Err(error) => {
                    warn!(%error, "cached upload no longer resolves, searching again");
                }
            }
        }

        let text = format!("{} {}", wanted.artist(), wanted.title);
        for source in SearchSource::ORDER {
            info!(query = ?text, source = source.name(), "searching for audio");
            let resolved = match self.resolve_input(&source.input(&text)).await {
                Ok(resolved) => resolved,
                Err(error @ (AudioError::Spawn { .. } | AudioError::Timeout(_))) => {
                    return Err(error);
                }
                // No results, unavailable, no Opus: try the next source.
                Err(error) => {
                    warn!(source = source.name(), error = %error, "search failed");
                    continue;
                }
            };
            if matching::matches(wanted, &resolved.metadata) {
                self.remember(wanted, &resolved).await;
                return Ok(resolved);
            }
            info!(source = source.name(), title = ?resolved.metadata.title,
                duration_secs = resolved.metadata.duration.map(|d| d.as_secs()),
                "search result has the wrong duration");
        }
        warn!(query = ?text, "no search result matched");
        Err(AudioError::NoMatch(text))
    }

    /// Record which upload a track resolved to, so the next run skips the
    /// search entirely.
    async fn remember(&self, wanted: &SpotifyTrack, resolved: &Resolved) {
        let (Some(cache), Some(url)) = (self.cache.clone(), resolved.metadata.webpage_url.clone())
        else {
            return;
        };
        let id = wanted.id.clone();
        // Committing fsyncs, so keep it off the runtime's worker threads.
        let _ = tokio::task::spawn_blocking(move || cache.put(&id, &url)).await;
    }

    /// Run `yt-dlp -j` and turn its JSON into a [`Resolved`].
    async fn resolve_input(&self, input: &str) -> Result<Resolved, AudioError> {
        let started = Instant::now();
        let mut cmd = Command::new(&self.config.program);

        cmd.arg("-j")
            .arg("--no-playlist")
            .arg("--no-warnings")
            .arg("--no-progress")
            .arg("-f")
            .arg(OPUS_FORMAT)
            // Searches resolve their top result only; no-op for a video.
            .args(["--playlist-end", "1"]);

        for arg in &self.config.extra_args {
            cmd.arg(arg);
        }

        let output = self.run(cmd, input).await?;
        let resolved = parse_output(&output)?;
        info!(title = ?resolved.metadata.title, duration_secs = resolved.metadata.duration.map(|d| d.as_secs()),
            elapsed_ms = started.elapsed().as_millis(), "audio source resolved");
        Ok(resolved)
    }

    /// Expand playlists without resolving each entry's expiring audio URL.
    pub async fn list(&self, url: &Url) -> Result<MediaList, AudioError> {
        let mut cmd = Command::new(&self.config.program);
        cmd.args(&self.config.extra_args)
            .args([
                "--dump-single-json",
                "--flat-playlist",
                "--yes-playlist",
                "--ignore-errors",
                "--no-warnings",
                "--no-progress",
                "--skip-download",
                "--playlist-end",
            ])
            .arg((PLAYLIST_LIMIT + 1).to_string())
            .args(["-f", OPUS_FORMAT]);
        let output = self.run(cmd, url.as_str()).await?;
        parse_list(&output, url)
    }

    async fn run(&self, mut cmd: Command, input: &str) -> Result<Vec<u8>, AudioError> {
        // `--` so a URL starting with `-` can never be read as an option.
        cmd.arg("--").arg(input);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If the timeout below fires, the future is dropped and this reaps
            // the child rather than leaking it.
            .kill_on_drop(true);

        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let child = cmd.spawn().map_err(|source| AudioError::Spawn {
            program: self.config.program.clone(),
            source,
        })?;

        let output = time::timeout(self.config.timeout, child.wait_with_output())
            .await
            .map_err(|_| AudioError::Timeout(self.config.timeout))?
            .map_err(|source| AudioError::Spawn {
                program: self.config.program.clone(),
                source,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // yt-dlp exits non-zero when the format selector matches nothing,
            // which for us means "no Opus rendition".
            if stderr.contains("Requested format is not available") {
                return Err(AudioError::NoOpusFormat);
            }

            return Err(AudioError::YtDlpFailed {
                code: output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                stderr: last_line(&stderr),
            });
        }

        Ok(output.stdout)
    }
}

fn parse_list(stdout: &[u8], original: &Url) -> Result<MediaList, AudioError> {
    let value: serde_json::Value = serde_json::from_slice(stdout)?;
    let Some(entries) = value.get("entries").and_then(serde_json::Value::as_array) else {
        let resolved = parse_output(stdout)?;
        return Ok(MediaList {
            tracks: vec![ListedTrack {
                target: original.clone().into(),
                metadata: resolved.metadata.clone(),
            }],
            skipped: 0,
            truncated: false,
            single: Some(resolved),
        });
    };
    let mut tracks = Vec::new();
    let mut skipped = 0;
    for entry in entries.iter().take(PLAYLIST_LIMIT) {
        let unavailable = matches!(
            entry.get("availability").and_then(|v| v.as_str()),
            Some("private" | "premium_only" | "subscriber_only" | "needs_auth")
        );
        let url = ["webpage_url", "url"]
            .into_iter()
            .filter_map(|key| entry.get(key).and_then(|v| v.as_str()))
            .find_map(|raw| YtDlp::validate_url(raw).ok());
        let Some(url) = url.filter(|_| !unavailable) else {
            skipped += 1;
            continue;
        };
        tracks.push(ListedTrack {
            metadata: TrackMetadata {
                title: entry
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                duration: entry
                    .get("duration")
                    .and_then(|v| v.as_f64())
                    .filter(|d| d.is_finite() && *d > 0.0)
                    .and_then(|d| Duration::try_from_secs_f64(d).ok()),
                // Flat entries list thumbnails smallest first.
                thumbnail: entry
                    .get("thumbnails")
                    .and_then(|v| v.as_array())
                    .and_then(|thumbnails| thumbnails.last())
                    .and_then(|thumbnail| thumbnail.get("url"))
                    .or_else(|| entry.get("thumbnail"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                webpage_url: Some(url.to_string()),
                ..TrackMetadata::default()
            },
            target: url.into(),
        });
    }
    Ok(MediaList {
        tracks,
        skipped,
        truncated: entries.len() > PLAYLIST_LIMIT,
        single: None,
    })
}

/// Parse a single `yt-dlp -j` record.
pub fn parse_output(stdout: &[u8]) -> Result<Resolved, AudioError> {
    let line = stdout
        .split(|b| *b == b'\n')
        .find(|l| !l.iter().all(u8::is_ascii_whitespace))
        .ok_or(AudioError::NoMetadata)?;

    // `-j` emits one object per line, so the first line is the record. Fall
    // back to the whole buffer in case the output is ever pretty-printed.
    let output: Output = match serde_json::from_slice(line) {
        Ok(output) => output,
        Err(line_error) => serde_json::from_slice(stdout).map_err(|_| line_error)?,
    };

    // With `-f` selecting one format, yt-dlp reports it either under
    // `requested_downloads` or flattened onto the top level.
    let format = output
        .requested_downloads
        .and_then(|mut d| (!d.is_empty()).then(|| d.swap_remove(0)))
        .filter(|f| f.url.is_some())
        .unwrap_or(output.format);

    let acodec_is_opus = format
        .acodec
        .as_deref()
        .is_some_and(|c| c.to_ascii_lowercase().starts_with("opus"));

    if !acodec_is_opus {
        return Err(AudioError::NoOpusFormat);
    }

    let media_url = format.url.ok_or(AudioError::NoMediaUrl)?;

    debug!(
        format_id = format.format_id.as_deref().unwrap_or("?"),
        abr = format.abr,
        "resolved opus format"
    );

    Ok(Resolved {
        media_url,
        headers: build_headers(format.http_headers.as_ref()),
        content_length: format.filesize.or(format.filesize_approx),
        bitrate_bps: format
            .abr
            .or(format.tbr)
            .filter(|b| *b > 0.0)
            .map(kbps_to_bps),
        metadata: TrackMetadata {
            title: output.title,
            duration: output
                .duration
                .filter(|d| *d > 0.0)
                .map(Duration::from_secs_f64),
            // Credited artists beat a "Name - Topic" channel.
            uploader: output
                .artists
                .filter(|a| !a.is_empty())
                .map(|a| a.join(", "))
                .or(output.uploader),
            thumbnail: output.thumbnail,
            webpage_url: output.webpage_url,
            requester: None,
            sequence: 0,
        },
    })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn kbps_to_bps(kbps: f64) -> u32 {
    (kbps * 1000.0).clamp(0.0, f64::from(u32::MAX)) as u32
}

/// Carry yt-dlp's `http_headers` over to our own client.
///
/// googlevideo cares about the `User-Agent` matching the one used to sign the
/// URL. `Range` is dropped because the stream layer sets its own.
fn build_headers(raw: Option<&BTreeMap<String, String>>) -> HeaderMap {
    let mut headers = HeaderMap::new();

    let Some(raw) = raw else {
        return headers;
    };

    for (name, value) in raw {
        let Ok(name) = HeaderName::try_from(name.as_str()) else {
            continue;
        };

        if name == RANGE {
            continue;
        }

        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    }

    headers
}

fn last_line(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no output")
        .trim()
        .to_owned()
}

#[derive(Deserialize)]
struct Output {
    title: Option<String>,
    duration: Option<f64>,
    webpage_url: Option<String>,
    uploader: Option<String>,
    artists: Option<Vec<String>>,
    thumbnail: Option<String>,
    requested_downloads: Option<Vec<Format>>,
    #[serde(flatten)]
    format: Format,
}

#[derive(Deserialize)]
struct Format {
    url: Option<String>,
    acodec: Option<String>,
    abr: Option<f64>,
    tbr: Option<f64>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    format_id: Option<String>,
    http_headers: Option<BTreeMap<String, String>>,
}

#[cfg(test)]
#[path = "../../tests/audio/ytdlp.rs"]
mod tests;
