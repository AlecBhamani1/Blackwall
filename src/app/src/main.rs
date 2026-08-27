#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use commands::AppState;
use thiserror::Error;

fn main() {
    if let Err(error) = run() {
        eprintln!("Blackwall could not start: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), StartupError> {
    tauri::Builder::default()
        .manage(AppState::new()?)
        .invoke_handler(tauri::generate_handler![
            commands::discover_models,
            commands::chat,
            commands::stream_chat,
            commands::start_share,
            commands::share_status,
            commands::stop_share,
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}

#[derive(Debug, Error)]
enum StartupError {
    #[error("could not initialize the local model client: {0}")]
    ModelClient(#[from] reqwest::Error),
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
}
