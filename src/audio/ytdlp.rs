//! yt-dlp invocation.
//!
//! yt-dlp is only ever used to *resolve* a video into a direct media URL plus
//! the headers needed to fetch it; the bytes themselves are pulled by
//! [`crate::audio::stream`]. The process is spawned as argv — there is no shell
//! anywhere in this path.

use std::{collections::BTreeMap, process::Stdio, time::Duration, time::Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};
use serde::Deserialize;
use tokio::{process::Command, time};
use tracing::{debug, info, warn};
use url::Url;

use super::matching::{self, SearchQuery};
use crate::{config::YtDlpConfig, error::AudioError};

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
}

#[derive(Debug, Clone)]
pub struct YtDlp {
    config: YtDlpConfig,
}

/// Limit imports so accidental channel/radio links cannot create an unbounded queue.
pub const PLAYLIST_LIMIT: usize = 500;

/// Searches are kept lazy, just like direct video URLs.
#[derive(Debug, Clone)]
pub enum PlaybackTarget {
    Url(Url),
    Search(SearchQuery),
}

/// Where searches look, best source first.
#[derive(Debug, Clone, Copy)]
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
        Self { config }
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
            PlaybackTarget::Search(query) => self.resolve_search(query).await,
        }
    }

    /// Resolve each source's top result until one runs about as long as the
    /// requested track.
    async fn resolve_search(&self, query: &SearchQuery) -> Result<Resolved, AudioError> {
        let text = query.text();
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
            if matching::matches(query, &resolved.metadata) {
                return Ok(resolved);
            }
            info!(source = source.name(), title = ?resolved.metadata.title,
                duration_secs = resolved.metadata.duration.map(|d| d.as_secs()),
                "search result has the wrong duration");
        }
        warn!(query = ?text, "no search result matched");
        Err(AudioError::NoMatch(text))
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
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
mod tests {
    use super::*;

    /// Fake yt-dlp: resolves each search input to its top result, and logs
    /// every input so the tests can check which ones were run, in order.
    #[cfg(unix)]
    const FAKE_YTDLP: &str = r##"#!/bin/sh
before=; last=
for arg; do before=$last; last=$arg; done
# The input is always one literal argument after `--`.
[ "$before" = '--' ] || exit 7
[ "$1" = '-j' ] && [ "$6" = 'bestaudio[acodec=opus]' ] || exit 3
printf '%s\n' "$last" >> "$0.log"
case "$last" in
'https://music.youtube.com/search?q=Artist+Song+%24%28literal%29%3B#songs')
  printf '%s\n' '{"title":"Song $(literal);","duration":181,"artists":["Artist"],"uploader":"Artist - Topic","webpage_url":"https://www.youtube.com/watch?v=song","url":"https://cdn/song","acodec":"opus"}' ;;
'https://music.youtube.com/search?q=Artist+Rare#songs')
  printf '%s\n' '{"title":"Rare (Extended Mix)","duration":400,"url":"https://cdn/extended","acodec":"opus"}' ;;
'ytsearch1:Artist Rare')
  printf '%s\n' '{"title":"Artist - Rare (Official Audio)","duration":182,"uploader":"ArtistVEVO","url":"https://cdn/rare","acodec":"opus"}' ;;
*) echo 'ERROR: no results' >&2; exit 1 ;;
esac
"##;

    #[cfg(unix)]
    #[tokio::test]
    async fn search_prefers_youtube_music_and_falls_back_on_wrong_duration() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "rust-music-bot-ytdlp-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let log = path.with_extension("log");
        std::fs::write(&path, FAKE_YTDLP).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ytdlp = YtDlp::new(YtDlpConfig {
            program: path.to_string_lossy().into_owned(),
            timeout: Duration::from_secs(5),
            extra_args: Vec::new(),
        });
        let search = |title: &str| {
            PlaybackTarget::Search(SearchQuery {
                artists: vec!["Artist".into()],
                title: title.into(),
                duration: Some(Duration::from_secs(180)),
            })
        };
        let calls = || {
            let calls = std::fs::read_to_string(&log).unwrap_or_default();
            std::fs::remove_file(&log).ok();
            calls.lines().map(str::to_owned).collect::<Vec<_>>()
        };

        // Shell syntax in a song name must reach yt-dlp as literal text.
        let found = ytdlp.resolve(&search("Song $(literal);")).await;
        let found_calls = calls();
        // YouTube Music's top result is an extended mix.
        let rare = ytdlp.resolve(&search("Rare")).await;
        let rare_calls = calls();
        let missing = ytdlp.resolve(&search("Missing")).await;
        let missing_calls = calls();
        std::fs::remove_file(&path).unwrap();

        let found = found.unwrap();
        assert_eq!(found.media_url, "https://cdn/song");
        assert_eq!(found.metadata.uploader.as_deref(), Some("Artist"));
        assert_eq!(
            found_calls,
            ["https://music.youtube.com/search?q=Artist+Song+%24%28literal%29%3B#songs"]
        );
        assert_eq!(rare.unwrap().media_url, "https://cdn/rare");
        assert_eq!(
            rare_calls,
            [
                "https://music.youtube.com/search?q=Artist+Rare#songs",
                "ytsearch1:Artist Rare",
            ]
        );
        assert!(matches!(missing, Err(AudioError::NoMatch(ref q)) if q == "Artist Missing"));
        assert_eq!(missing_calls.len(), 2);
    }

    /// Real searches where the top result has to be checked. Opt-in:
    ///   cargo test live_search -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires yt-dlp and network access"]
    async fn live_search_finds_the_requested_recording() {
        let ytdlp = YtDlp::new(YtDlpConfig {
            program: std::env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_owned()),
            timeout: Duration::from_secs(60),
            extra_args: Vec::new(),
        });
        for (artists, title, secs) in [
            (
                &["Daft Punk", "Pharrell Williams", "Nile Rodgers"][..],
                "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
                369,
            ),
            (
                &["Daft Punk", "Pharrell Williams", "Nile Rodgers"][..],
                "Get Lucky - Radio Edit",
                248,
            ),
            (&["Beyoncé"][..], "Halo", 261),
            (&["Oasis"][..], "Live Forever - Remastered", 277),
        ] {
            let query = SearchQuery {
                artists: artists.iter().map(|&a| a.to_owned()).collect(),
                title: title.into(),
                duration: Some(Duration::from_secs(secs)),
            };
            let started = Instant::now();
            let resolved = ytdlp
                .resolve(&PlaybackTarget::Search(query.clone()))
                .await
                .unwrap();
            println!(
                "{title}: {:?} by {:?}, {:?}, {:?} in {:?}",
                resolved.metadata.title,
                resolved.metadata.uploader,
                resolved.metadata.duration,
                resolved.metadata.webpage_url,
                started.elapsed()
            );
            assert!(matching::matches(&query, &resolved.metadata));
        }
    }

    #[test]
    fn playlist_preserves_order_and_skips_invalid_or_unavailable_entries() {
        let json = br#"{"entries":[
            {"url":"https://example.com/first","title":"First","duration":123.5},
            null,
            {"url":"file:///secret"},
            {"url":"https://example.com/private","availability":"private"},
            {"webpage_url":"https://example.com/last","url":"https://cdn.example.com/audio","title":"Last","duration":-1}
        ]}"#;
        let list = parse_list(json, &Url::parse("https://example.com/playlist").unwrap()).unwrap();
        assert_eq!(list.skipped, 3);
        assert_eq!(list.tracks.len(), 2);
        assert_eq!(list.tracks[0].metadata.title.as_deref(), Some("First"));
        assert_eq!(
            list.tracks[0].metadata.duration,
            Some(Duration::from_millis(123500))
        );
        assert_eq!(
            match &list.tracks[1].target {
                PlaybackTarget::Url(url) => url.as_str(),
                _ => panic!("expected URL"),
            },
            "https://example.com/last"
        );
        assert_eq!(list.tracks[1].metadata.duration, None);
        assert!(list.single.is_none());
        assert!(!list.truncated);
    }

    #[test]
    fn playlist_limit_and_empty_playlists() {
        let url = Url::parse("https://example.com/playlist").unwrap();
        let list = parse_list(br#"{"entries":[]}"#, &url).unwrap();
        assert!(list.tracks.is_empty());
        let entries: Vec<_> = (0..PLAYLIST_LIMIT + 1)
            .map(|i| serde_json::json!({"url":format!("https://example.com/{i}")}))
            .collect();
        let json = serde_json::to_vec(&serde_json::json!({"entries":entries})).unwrap();
        let list = parse_list(&json, &url).unwrap();
        assert!(list.truncated);
        assert_eq!(list.tracks.len(), PLAYLIST_LIMIT);
    }

    #[test]
    fn single_track_listing_reuses_resolved_audio() {
        let url = Url::parse("https://example.com/watch").unwrap();
        let list = parse_list(
            br#"{"title":"Song","url":"https://cdn.example.com/audio","acodec":"opus"}"#,
            &url,
        )
        .unwrap();
        assert!(matches!(&list.tracks[0].target, PlaybackTarget::Url(actual) if actual == &url));
        assert_eq!(
            list.single.unwrap().media_url,
            "https://cdn.example.com/audio"
        );
    }

    #[test]
    fn accepts_http_urls_from_any_host() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://example.com/watch?v=x",
            "http://example.com/audio.opus",
        ] {
            assert!(YtDlp::validate_url(url).is_ok(), "{url} should be accepted");
        }
    }

    #[test]
    fn rejects_invalid_urls_and_other_schemes() {
        for url in [
            "file:///etc/passwd",
            "not a url",
            "ftp://example.com/audio.opus",
            "https://",
        ] {
            assert!(
                YtDlp::validate_url(url).is_err(),
                "{url} should be rejected"
            );
        }
    }

    /// The real shape of `yt-dlp -j`: one object on one line, trailing newline.
    #[test]
    fn parses_flattened_opus_format() {
        let json = br#"{"title":"Example","duration":212.0,"uploader":"Someone","webpage_url":"https://www.youtube.com/watch?v=x","url":"https://rr1---sn-x.googlevideo.com/videoplayback?x=1","acodec":"opus","abr":160.0,"filesize":4194304,"format_id":"251","http_headers":{"User-Agent":"Mozilla/5.0","Range":"bytes=0-"}}
"#;

        let resolved = parse_output(json).expect("should parse");

        assert_eq!(resolved.bitrate_bps, Some(160_000));
        assert_eq!(resolved.content_length, Some(4_194_304));
        assert_eq!(resolved.metadata.title.as_deref(), Some("Example"));
        assert_eq!(resolved.metadata.duration, Some(Duration::from_secs(212)));
        assert!(resolved.headers.contains_key("user-agent"));
        // Our own Range header must win.
        assert!(!resolved.headers.contains_key(RANGE));
    }

    /// Pretty-printed output still parses via the whole-buffer fallback.
    #[test]
    fn prefers_requested_downloads() {
        let json = br#"{
            "title": "Example",
            "url": "https://example.invalid/top-level",
            "acodec": "opus",
            "requested_downloads": [
                {"url": "https://example.invalid/chosen", "acodec": "opus", "abr": 128.0}
            ]
        }"#;

        let resolved = parse_output(json).expect("should parse");

        assert_eq!(resolved.media_url, "https://example.invalid/chosen");
        assert_eq!(resolved.bitrate_bps, Some(128_000));
    }

    #[test]
    fn rejects_non_opus_audio() {
        let json =
            br#"{"title":"x","url":"https://example.invalid/a","acodec":"mp4a.40.2","abr":128.0}"#;

        assert!(matches!(parse_output(json), Err(AudioError::NoOpusFormat)));
    }

    #[test]
    fn errors_on_empty_output() {
        assert!(matches!(
            parse_output(b"\n  \n"),
            Err(AudioError::NoMetadata)
        ));
    }
}
