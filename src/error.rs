//! Error types for start-up configuration. The audio and media pipelines
//! keep theirs next to their code.

use thiserror::Error;

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
