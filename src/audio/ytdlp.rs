//! yt-dlp invocation.
//!
//! yt-dlp is only ever used to *resolve* a video into a direct media URL plus
//! the headers needed to fetch it; the bytes themselves are pulled by
//! [`crate::audio::stream`]. The process is spawned as argv — there is no shell
//! anywhere in this path.

use std::{collections::BTreeMap, process::Stdio, time::Duration};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};
use serde::Deserialize;
use tokio::{process::Command, time};
use tracing::debug;
use url::Url;

use crate::{config::YtDlpConfig, error::AudioError};

/// Opus-only format selection.
///
/// 251/250/249 are YouTube's Opus-in-WebM renditions (high/medium/low). The
/// trailing generic selector covers the rare case where those itags are absent
/// but some other Opus rendition exists. Nothing here can select a format that
/// would need transcoding.
const OPUS_FORMAT: &str = "251/250/249/bestaudio[acodec=opus]";

/// Hosts we are willing to hand to yt-dlp.
///
/// Passing argv means there is no injection risk, but this keeps the bot from
/// being pointed at yt-dlp's several hundred other extractors.
const ALLOWED_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
    "www.youtu.be",
];

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

impl YtDlp {
    pub fn new(config: YtDlpConfig) -> Self {
        Self { config }
    }

    /// Check that `raw` is a YouTube link we are willing to resolve.
    pub fn validate_url(raw: &str) -> Result<Url, AudioError> {
        let url = Url::parse(raw.trim()).map_err(|_| AudioError::UnsupportedUrl(raw.to_owned()))?;

        let scheme_ok = matches!(url.scheme(), "http" | "https");
        let host_ok = url
            .host_str()
            .is_some_and(|host| ALLOWED_HOSTS.contains(&host.to_ascii_lowercase().as_str()));

        if scheme_ok && host_ok {
            Ok(url)
        } else {
            Err(AudioError::UnsupportedUrl(raw.to_owned()))
        }
    }

    /// Run `yt-dlp -j` and turn its JSON into a [`Resolved`].
    pub async fn resolve(&self, url: &Url) -> Result<Resolved, AudioError> {
        let mut cmd = Command::new(&self.config.program);

        cmd.arg("-j")
            .arg("--no-playlist")
            .arg("--no-warnings")
            .arg("--no-progress")
            .arg("-f")
            .arg(OPUS_FORMAT);

        for arg in &self.config.extra_args {
            cmd.arg(arg);
        }

        // `--` so a URL starting with `-` can never be read as an option.
        cmd.arg("--").arg(url.as_str());

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

        parse_output(&output.stdout)
    }
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
            uploader: output.uploader,
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

    #[test]
    fn accepts_youtube_urls() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert!(YtDlp::validate_url(url).is_ok(), "{url} should be accepted");
        }
    }

    #[test]
    fn rejects_other_hosts_and_schemes() {
        for url in [
            "https://example.com/watch?v=x",
            "file:///etc/passwd",
            "not a url",
            "https://youtube.com.evil.test/watch?v=x",
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
