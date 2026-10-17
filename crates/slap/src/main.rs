//! Headless host: run the bot until Ctrl-C (or SIGINT from Docker).

use std::process::ExitCode;

use slap_core::{Bot, BoxError, Config};
use tokio::signal;
use tracing::{error, info, warn};

fn main() -> ExitCode {
    let dotenv = dotenvy::dotenv();

    let _log_guard = match slap_core::logging::init() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    info!("starting...");

    match dotenv {
        Ok(path) => info!(path = %path.display(), "loaded .env"),
        // No `.env` is the normal case in production.
        Err(error) if error.not_found() => {}
        Err(error) => warn!(%error, "could not read .env; using the environment as-is"),
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            error!(%error, "invalid configuration");

            return ExitCode::FAILURE;
        }
    };

    let runtime = match slap_core::build_runtime(config.worker_threads) {
        Ok(runtime) => runtime,
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

    bot.shutdown().await;

    Ok(())
}
