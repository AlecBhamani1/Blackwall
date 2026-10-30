#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_commands;
mod auth;
mod commands;
mod credentials;
mod data;
mod identity;
mod keychain;
#[cfg(target_os = "macos")]
mod lifecycle;
mod pairing;
mod project;
mod setup;
mod updates;

use blackwall_core::secrets;
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
    #[cfg(target_os = "macos")]
    lifecycle::adopt_login_path();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new()?)
        .manage(setup::SetupState::default())
        .manage(agent_commands::AgentState::default())
        .manage(auth::AuthState::default())
        .manage(pairing::PairingState::default())
        .manage(data::TelemetryState::default());
    #[cfg(target_os = "macos")]
    let builder = builder.on_window_event(lifecycle::on_window_event);
    let app = builder
        .setup(|app| {
            // Find bubblewrap now, off the main thread, so the first command doesn't wait for it.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Ok(directory) = identity::data_directory(&handle) {
                    let _ = blackwall_core::sandbox::sandbox_status(&directory);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            updates::check_app_update,
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
            agent_commands::restore_workspace,
            agent_commands::browse_workspace,
            agent_commands::search_workspace,
            agent_commands::read_workspace_file,
            agent_commands::resolve_approval,
            agent_commands::export_conversation,
            project::project_status,
            project::trust_project,
            project::list_trusted_projects,
            project::untrust_project,
            data::list_skills,
            data::save_skill,
            data::delete_skill,
            data::list_skill_proposals,
            data::approve_skill_proposal,
            data::reject_skill_proposal,
            data::find_skill_imports,
            data::choose_skill_import,
            data::import_skill,
            data::list_suggested_skills,
            data::get_suggested_skill,
            data::discard_skill_downloads,
            data::list_traces,
            data::read_trace,
            data::usage_summary,
            data::export_telemetry,
            data::load_telemetry,
            data::save_telemetry,
            data::save_telemetry_header,
            data::delete_telemetry_header,
            data::telemetry_status,
            data::test_telemetry,
            data::list_mcp_servers,
            data::add_mcp_server,
            data::add_mcp_http_server,
            data::set_mcp_server_enabled,
            data::remove_mcp_server,
            data::save_mcp_secret,
            data::delete_mcp_secret,
            data::list_mcp_tool_rules,
            data::remove_mcp_tool_rule,
            data::list_sessions,
            data::search_sessions,
            data::load_session,
            data::save_session,
            data::delete_session,
            data::migrate_sessions,
            data::list_memories,
            data::save_memory,
            data::delete_memory,
            data::list_memory_proposals,
            data::approve_memory_proposal,
            data::reject_memory_proposal,
            data::load_preferences,
            data::save_preferences,
            credentials::save_model_key,
            credentials::has_saved_credential,
            credentials::remove_saved_credential,
            setup::discover_local_services,
            setup::detect_server,
            setup::download_local_model,
            setup::cancel_request,
            setup::open_setup_link,
            commands::sandbox_status,
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
        .build(context)?;
    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event {
            tauri::Manager::state::<data::TelemetryState>(handle).shutdown();
        }
        #[cfg(target_os = "macos")]
        lifecycle::on_run_event(handle, event);
    });
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
