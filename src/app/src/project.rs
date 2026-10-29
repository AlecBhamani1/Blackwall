//! Review and trust the selected project's `.blackwall/project.json`. Only the selected project
//! can be trusted, and only the exact file the owner was shown.
use crate::agent_commands::{selected_workspace, AgentState};
use blackwall_core::project::{self, ProjectStatus, TrustedProject};
use std::path::Path;
use tauri::{AppHandle, State};

async fn with_data<T: Send + 'static>(
    app: &AppHandle,
    operation: impl FnOnce(&Path) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    crate::auth::require_unlocked(app).await?;
    let directory = crate::identity::data_directory(app)?;
    tokio::task::spawn_blocking(move || operation(&directory))
        .await
        .map_err(|_| "The project settings task could not finish.")?
}

#[tauri::command]
pub async fn project_status(
    app: AppHandle,
    state: State<'_, AgentState>,
    workspace: String,
) -> Result<ProjectStatus, String> {
    crate::auth::require_unlocked(&app).await?;
    let folder = selected_workspace(&state, &workspace)?.path;
    with_data(&app, move |directory| {
        Ok(project::status(&folder, directory))
    })
    .await
}

/// Trust the file only if it still has the hash shown in the dialog.
#[tauri::command]
pub async fn trust_project(
    app: AppHandle,
    state: State<'_, AgentState>,
    workspace: String,
    sha256: String,
) -> Result<ProjectStatus, String> {
    crate::auth::require_unlocked(&app).await?;
    let folder = selected_workspace(&state, &workspace)?.path;
    with_data(&app, move |directory| {
        project::trust(&folder, directory, &sha256).map_err(|error| error.to_string())?;
        Ok(project::status(&folder, directory))
    })
    .await
}

#[tauri::command]
pub async fn list_trusted_projects(app: AppHandle) -> Result<Vec<TrustedProject>, String> {
    with_data(&app, |directory| {
        project::trusted(directory).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
pub async fn untrust_project(app: AppHandle, path: String) -> Result<(), String> {
    with_data(&app, move |directory| {
        project::untrust(directory, &path)
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await
}
