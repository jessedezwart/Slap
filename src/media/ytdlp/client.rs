//! Client for yt-dlp

use std::process::Stdio;

use tokio::{process::Command, time};
use tracing::warn;
use url::Url;

use super::{YtDlpError, parse};
use crate::{
    config::YtDlpConfig,
    media::{Import, PLAYLIST_LIMIT},
};

/// Best available Opus quality
const OPUS_FORMAT: &str = "bestaudio[acodec=opus]";

/// Keeps a console window from flashing up per yt-dlp invocation on Windows.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone)]
pub struct YtDlpClient {
    config: YtDlpConfig,
}

impl YtDlpClient {
    pub fn new(config: YtDlpConfig) -> Self {
        Self { config }
    }

    /// List a link: one video, or the entries of a playlist.
    pub async fn import(&self, url: &Url) -> Result<Import, YtDlpError> {
        let output = self.playlist_json(url).await?;
        // A single video comes back with its stream already attached.
        parse::parse_list(&output, url)
    }

    pub(super) async fn video_json(&self, input: &str) -> Result<Vec<u8>, YtDlpError> {
        let mut cmd = Command::new(&self.config.program);

        // Output json
        cmd.arg("-j")
            .arg("--no-playlist")
            .arg("--no-warnings")
            .arg("-f")
            .arg(OPUS_FORMAT)
            // Searches resolve their top result only; no-op for a video.
            .args(["--playlist-end", "1"])
            .args(&self.config.extra_args);

        self.run(cmd, input).await
    }

    async fn playlist_json(&self, url: &Url) -> Result<Vec<u8>, YtDlpError> {
        let mut cmd = Command::new(&self.config.program);
        cmd.args([
            "--dump-single-json",
            "--flat-playlist",
            "--yes-playlist",
            // --ignore-errors is needed to get a partial listing for a playlist
            "--ignore-errors",
            "--no-warnings",
            "--skip-download",
            "--playlist-end",
        ])
        .arg((PLAYLIST_LIMIT + 1).to_string())
        .args(["-f", OPUS_FORMAT])
        .args(&self.config.extra_args);
        self.run(cmd, url.as_str()).await
    }

    async fn run(&self, mut cmd: Command, input: &str) -> Result<Vec<u8>, YtDlpError> {
        if self.config.cookies_file.is_file() {
            cmd.arg("--cookies").arg(&self.config.cookies_file);
        }
        // Parse input always as an option
        cmd.arg("--").arg(input);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Kill the child if the pipeline is dropped.
            .kill_on_drop(true);

        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let child = cmd.spawn().map_err(|source| YtDlpError::Spawn {
            program: self.config.program.clone(),
            source,
        })?;

        let output = time::timeout(self.config.timeout, child.wait_with_output())
            .await
            // Map_err is used here to convert the timeout error into a YtDlpError::Timeout variant
            .map_err(|_| YtDlpError::Timeout(self.config.timeout))?
            // Map the result of wait_with_output to a YtDlpError::Spawn variant if it fails
            .map_err(|source| YtDlpError::Spawn {
                program: self.config.program.clone(),
                source,
            })?;

        if !output.status.success() {
            let code = output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |c| c.to_string());
            let stderr = last_error_line(&output.stderr);
            warn!(%code, %stderr, input, "yt-dlp failed");
            return Err(YtDlpError::Failed);
        }

        Ok(output.stdout)
    }
}

fn last_error_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        // Find the last non-empty line that doesn't start with "[PYI-"
        // to prevent logging "[PYI-4821:ERROR] Failed to execute script 'yt-dlp'" for example.
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("[PYI-"))
        .unwrap_or("no output")
        .trim()
        .to_string()
}
