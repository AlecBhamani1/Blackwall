//! Native setup commands. Local probes/downloads are isolated from model credentials.
use blackwall_core::{
    connection::{self, DownloadProgress, LocalService},
    jobs::Jobs,
};
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};

#[derive(Default)]
pub struct SetupState {
    pub jobs: Jobs,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent {
    request_id: String,
    #[serde(flatten)]
    progress: DownloadProgress,
}

#[tauri::command]
pub async fn discover_local_services() -> Vec<LocalService> {
    connection::discover_local_services().await
}

#[tauri::command]
pub async fn download_local_model(
    model: String,
    request_id: String,
    app: AppHandle,
    state: State<'_, SetupState>,
) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    let mut job = state.jobs.start(&request_id)?;
    let download = connection::download_model(&model, |progress| {
        let _ = app.emit(
            "blackwall://download",
            ProgressEvent {
                request_id: request_id.clone(),
                progress,
            },
        );
    });
    tokio::select! {
        biased;
        _ = job.cancelled() => Err("Download stopped.".into()),
        result = tokio::time::timeout(Duration::from_secs(2 * 60 * 60), download) =>
            result.map_err(|_| "The download took too long. Try again.")?.map_err(|e| e.to_string()),
    }
}

#[tauri::command]
pub fn cancel_request(request_id: String, state: State<'_, SetupState>) -> Result<(), String> {
    state.jobs.cancel(&request_id)
}

const OLLAMA_DOWNLOAD: &str = if cfg!(target_os = "macos") {
    "https://ollama.com/download/mac"
} else if cfg!(windows) {
    "https://ollama.com/download/windows"
} else {
    "https://ollama.com/download/linux"
};

#[tauri::command]
pub async fn open_setup_link(kind: String, invitation: Option<String>) -> Result<(), String> {
    let target = match kind.as_str() {
        "download" => OLLAMA_DOWNLOAD.to_owned(),
        "invitation" => connection::validate_invitation(invitation.as_deref().unwrap_or_default())
            .map_err(|_| {
                "Paste the complete invitation link from Blackwall, including its access key."
            })?
            .to_string(),
        _ => return Err("Unknown setup action.".into()),
    };
    // Fixed system openers receive one validated URL argument, never shell syntax.
    #[cfg(target_os = "macos")]
    let mut opener = tokio::process::Command::new("/usr/bin/open");
    #[cfg(windows)]
    let mut opener = {
        let mut command = tokio::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut opener = tokio::process::Command::new("xdg-open");
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        opener.arg(&target).kill_on_drop(true).status(),
    )
    .await
    .map_err(|_| "The browser took too long to open.")?
    .map_err(|_| "Your browser could not be opened. Open this link in your browser instead.")?;
    if status.success() {
        Ok(())
    } else {
        Err("Your browser could not be opened. Open this link in your browser instead.".into())
    }
}
