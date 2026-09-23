//! Client for yt-dlp

use std::process::Stdio;

use tokio::{process::Command, time};
use url::Url;

use super::PLAYLIST_LIMIT;
use crate::{config::YtDlpConfig, error::AudioError};

/// Pick best available Opus quality
const OPUS_FORMAT: &str = "bestaudio[acodec=opus]";

/// Keeps a console window from flashing up per invocation on Windows.
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

    /// `yt-dlp -j`: one JSON record for a single video or a search's top hit.
    pub(super) async fn resolve(&self, input: &str) -> Result<Vec<u8>, AudioError> {
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

        self.run(cmd, input).await
    }

    /// Expand a playlist without resolving each entry's expiring audio URL.
    pub(super) async fn list(&self, url: &Url) -> Result<Vec<u8>, AudioError> {
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
        self.run(cmd, url.as_str()).await
    }

    async fn run(&self, mut cmd: Command, input: &str) -> Result<Vec<u8>, AudioError> {
        if let Some(path) = &self.config.cookies_file {
            cmd.arg("--cookies").arg(path);
        }
        // Keep the input literal, including anything that looks like an option.
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

        // --ignore-errors can leave a successful exit status after an extractor
        // failed, so inspect stderr even for playlist listings that exit zero.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lower = stderr.to_lowercase();
        if lower.contains("sign in to confirm")
            && (lower.contains("not a bot") || lower.contains("not a robot"))
        {
            return Err(AudioError::CookiesRequired);
        }
        if !output.status.success() {
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

/// Returns the last non-empty line of stderr, or "no output" if there is none.
fn last_line(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no output")
        .trim()
        .to_owned()
}
