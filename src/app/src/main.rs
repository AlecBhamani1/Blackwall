#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_commands;
mod auth;
mod commands;
mod credentials;
mod data;
mod identity;
mod keychain;
mod pairing;
mod setup;

use commands::AppState;
use thiserror::Error;

fn main() {
    if let Err(error) = run() {
        eprintln!("Blackwall could not start: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), StartupError> {
    let mut context = tauri::generate_context!();
    identity::initialize(&context.config().identifier).map_err(StartupError::Identity)?;
    identity::configure_acceptance_webviews(context.config_mut());
    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new()?)
        .manage(setup::SetupState::default())
        .manage(agent_commands::AgentState::default())
        .manage(auth::AuthState::default())
        .manage(pairing::PairingState::default())
        .invoke_handler(tauri::generate_handler![
            pairing::pairing_status,
            pairing::retry_paired_device,
            pairing::create_pairing,
            pairing::join_pairing,
            pairing::approve_pairing,
            pairing::cancel_pairing,
            pairing::remove_paired_device,
            auth::auth_status,
            auth::unlock_app,
            auth::set_passphrase,
            auth::lock_app,
            agent_commands::choose_workspace,
            agent_commands::resolve_approval,
            agent_commands::export_conversation,
            data::list_skills,
            data::save_skill,
            data::delete_skill,
            data::list_sessions,
            data::load_session,
            data::save_session,
            data::delete_session,
            data::migrate_sessions,
            data::list_memories,
            data::save_memory,
            data::delete_memory,
            data::load_preferences,
            data::save_preferences,
            credentials::save_model_key,
            credentials::has_saved_credential,
            credentials::remove_saved_credential,
            setup::discover_local_services,
            setup::download_local_model,
            setup::cancel_request,
            setup::open_setup_link,
            commands::model_endpoint,
            commands::discover_models,
            commands::chat,
            commands::stream_chat,
            commands::start_share,
            commands::share_status,
            commands::list_shares,
            commands::revoke_share,
            commands::stop_share,
        ])
        .run(context)?;
    Ok(())
}

#[derive(Debug, Error)]
enum StartupError {
    #[error("{0}")]
    Identity(String),
    #[error("could not initialize the local model client: {0}")]
    ModelClient(#[from] reqwest::Error),
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
}
