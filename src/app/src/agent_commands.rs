//! Project selection and approval resolution; no arbitrary filesystem paths from chat IPC.
use blackwall_core::{approvals::Approvals, protocol::ResolveApprovalRequest, tools::Workspace};
use std::sync::Mutex;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

#[derive(Default)]
pub struct AgentState {
    pub workspace: Mutex<Option<Workspace>>,
    pub approvals: Approvals,
}
#[tauri::command]
pub async fn choose_workspace(
    app: AppHandle,
    state: State<'_, AgentState>,
) -> Result<Option<String>, String> {
    crate::auth::require_unlocked(&app).await?;
    let picker = app.clone();
    let chosen = tokio::task::spawn_blocking(move || {
        picker
            .dialog()
            .file()
            .set_title("Choose a project folder for Blackwall")
            .blocking_pick_folder()
    })
    .await
    .map_err(|_| "The folder picker could not open.")?;
    crate::auth::require_unlocked(&app).await?;
    let Some(chosen) = chosen else {
        return Ok(None);
    };
    let path = chosen
        .into_path()
        .map_err(|_| "Choose a local project folder.")?;
    let workspace = Workspace::open(&path).map_err(|error| error.to_string())?;
    let label = workspace.path.to_string_lossy().into_owned();
    *state
        .workspace
        .lock()
        .map_err(|_| "The project could not be selected.")? = Some(workspace);
    Ok(Some(label))
}
#[tauri::command]
pub async fn resolve_approval(
    app: AppHandle,
    request: ResolveApprovalRequest,
    state: State<'_, AgentState>,
) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    state.approvals.resolve(request)
}
#[tauri::command]
pub async fn export_conversation(
    app: AppHandle,
    session: serde_json::Value,
) -> Result<bool, String> {
    crate::auth::require_unlocked(&app).await?;
    let body = serde_json::to_vec_pretty(&session)
        .map_err(|_| "This conversation could not be exported.")?;
    if body.len() > 64 * 1024 * 1024 {
        return Err("This conversation exceeds the export size limit.".into());
    }
    let picker = app.clone();
    let file = tokio::task::spawn_blocking(move || {
        picker
            .dialog()
            .file()
            .set_title("Export conversation")
            .set_file_name("Blackwall conversation.json")
            .add_filter("JSON", &["json"])
            .blocking_save_file()
    })
    .await
    .map_err(|_| "The save dialog could not open.")?;
    crate::auth::require_unlocked(&app).await?;
    let Some(file) = file else {
        return Ok(false);
    };
    tokio::fs::write(file.into_path().map_err(|_| "Choose a local file.")?, body)
        .await
        .map_err(|_| "The export could not be saved. Check disk space and folder permissions.")?;
    Ok(true)
}
