//! Desktop host: run the bot with a small status window.
//!
//! The bot lives on our own Tokio runtime, shared with Tauri. Closing the
//! window shuts the bot down cleanly before the process exits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Serialize;
use slap_core::{Bot, Config, GuildSummary};
use tauri::{Manager as _, RunEvent};
use tracing::{error, info, warn};

enum Status {
    Starting,
    Ready(Bot),
    Failed(String),
    Stopping,
}

type Shared = Arc<Mutex<Status>>;

/// What the window polls for.
#[derive(Serialize)]
struct Snapshot {
    status: &'static str,
    error: Option<String>,
    guilds: Vec<GuildSummary>,
}

#[tauri::command]
fn snapshot(status: tauri::State<'_, Shared>) -> Snapshot {
    let status = status.lock().unwrap();
    let (name, error, guilds) = match &*status {
        Status::Starting => ("starting", None, Vec::new()),
        Status::Ready(bot) => ("ready", None, bot.guilds()),
        Status::Failed(error) => ("failed", Some(error.clone()), Vec::new()),
        Status::Stopping => ("stopping", None, Vec::new()),
    };
    Snapshot {
        status: name,
        error,
        guilds,
    }
}

fn main() -> ExitCode {
    let dotenv = dotenvy::dotenv();

    let _log_guard = match slap_core::logging::init() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = dotenv
        && !error.not_found()
    {
        warn!(%error, "could not read .env; using the environment as-is");
    }

    // A bad config still opens the window, so the error is visible there.
    let config = Config::from_env().map_err(|error| {
        error!(%error, "invalid configuration");
        error.to_string()
    });

    let runtime = match slap_core::build_runtime(
        config
            .as_ref()
            .ok()
            .and_then(|config| config.worker_threads),
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            error!(%error, "could not start the tokio runtime");
            return ExitCode::FAILURE;
        }
    };
    tauri::async_runtime::set(runtime.handle().clone());

    let status: Shared = Arc::new(Mutex::new(match &config {
        Ok(_) => Status::Starting,
        Err(error) => Status::Failed(error.clone()),
    }));

    if let Ok(config) = config {
        let status = Arc::clone(&status);
        runtime.spawn(async move {
            let next = match Bot::start(config).await {
                Ok(bot) => {
                    info!("ready");
                    Status::Ready(bot)
                }
                Err(error) => {
                    error!(%error, "could not start the bot");
                    Status::Failed(error.to_string())
                }
            };
            let mut status = status.lock().unwrap();
            // The window may have closed while we were connecting.
            match &*status {
                Status::Stopping => {
                    if let Status::Ready(bot) = next {
                        tauri::async_runtime::spawn(bot.shutdown());
                    }
                }
                _ => *status = next,
            }
        });
    }

    let app = match tauri::Builder::default()
        .manage(status)
        .invoke_handler(tauri::generate_handler![snapshot])
        .build(tauri::generate_context!())
    {
        Ok(app) => app,
        Err(error) => {
            error!(%error, "could not start the window");
            return ExitCode::FAILURE;
        }
    };

    let code = app.run_return(|handle, event| {
        let RunEvent::ExitRequested { api, .. } = event else {
            return;
        };
        let status = handle.state::<Shared>();
        let previous = std::mem::replace(&mut *status.lock().unwrap(), Status::Stopping);
        // Keep the process alive until the bot has left its voice channels;
        // the `exit` below asks again, and then there is nothing to stop.
        if let Status::Ready(bot) = previous {
            api.prevent_exit();
            info!("shutting down");
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                bot.shutdown().await;
                handle.exit(0);
            });
        }
    });

    runtime.shutdown_timeout(Duration::from_secs(2));

    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
