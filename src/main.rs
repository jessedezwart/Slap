mod audio;
mod bot;
mod config;
mod discord;
mod media;
mod session;

use std::process::ExitCode;

use tokio::signal;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, field::MakeExt, fmt::format::debug_fn};

use crate::{bot::Bot, config::Config};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn main() -> ExitCode {
    // Initialize logging and load env vars.
    let dotenv = dotenvy::dotenv();
    // Songbird's info-level spans record whole configs and bury the log target.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,songbird=warn")),
        )
        // Color field values in terminals; the default only styles field names.
        .fmt_fields(
            debug_fn(|writer, field, value| match field.name() {
                "message" => write!(writer, "{value:?}"),
                name if writer.has_ansi_escapes() => {
                    write!(writer, "\x1b[2m{name}=\x1b[0m\x1b[36m{value:?}\x1b[0m")
                }
                name => write!(writer, "{name}={value:?}"),
            })
            .delimited(" "),
        )
        .init();

    info!("starting...");

    match dotenv {
        Ok(path) => info!(path = %path.display(), "loaded .env"),
        Err(error) if error.not_found() => {}
        Err(error) => warn!(%error, "could not read .env; using the environment as-is"),
    }

    // Load config
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            error!(%error, "invalid configuration");

            return ExitCode::FAILURE;
        }
    };
    info!("loaded config");

    // Start the tokio runtime
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => {
            info!(
                workers = runtime.metrics().num_workers(),
                "started tokio runtime"
            );
            runtime
        }
        Err(error) => {
            error!(%error, "could not start the tokio runtime");

            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(%error, "fatal error");

            ExitCode::FAILURE
        }
    }
}

async fn run(config: Config) -> Result<(), BoxError> {
    let bot = Bot::start(config).await?;
    info!("ready");

    signal::ctrl_c().await?;
    info!("shutting down");

    bot.stop().await;

    Ok(())
}
