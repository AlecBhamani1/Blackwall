//! Desktop setup and credential adapter; all research runs through shared core.
use blackwall_browser::PlaywrightRenderer;
use blackwall_core::web::{BraveSearch, SearchCredentials, WebRuntime, WebSettings};
use futures_util::future::BoxFuture;
use std::sync::Arc;
use tauri::{AppHandle, Manager};
const ACCOUNT: &str = "https://api.search.brave.com";
struct Credentials;
impl SearchCredentials for Credentials {
    fn brave_key(&self) -> BoxFuture<'_, Result<Option<String>, String>> {
        Box::pin(async {
            let service = crate::identity::keychain_service("com.blackwall.search");
            crate::keychain::read(move || {
                crate::secrets::get(&service, ACCOUNT)
                    .map_err(|e| e.explain("Brave's key could not be read."))
            })
            .await
        })
    }
}
pub fn initialize(app: &AppHandle) -> Result<(), String> {
    let directory = crate::identity::data_directory(app)?;
    let renderer = Arc::new(PlaywrightRenderer::new(directory.join("browser")));
    app.state::<crate::agent_commands::AgentState>()
        .approvals
        .set_web_runtime(Arc::new(WebRuntime::new(
            directory,
            Arc::new(Credentials),
            Some(renderer),
        )))
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebView {
    settings: WebSettings,
    has_key: bool,
    browser_supported: bool,
    browser_installed: bool,
}
#[tauri::command]
pub async fn load_web_settings(app: AppHandle) -> Result<WebView, String> {
    crate::auth::require_unlocked(&app).await?;
    let directory = crate::identity::data_directory(&app)?;
    let renderer = PlaywrightRenderer::new(directory.join("browser"));
    Ok(WebView {
        settings: WebSettings::load(&directory).await?,
        has_key: Credentials.brave_key().await?.is_some(),
        browser_supported: PlaywrightRenderer::supported(),
        browser_installed: renderer.installed(),
    })
}
#[tauri::command]
pub async fn save_web_settings(app: AppHandle, settings: WebSettings) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    settings.save(&crate::identity::data_directory(&app)?).await
}
#[tauri::command]
pub async fn save_search_key(app: AppHandle, key: String) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    if key.len() > 8192 || key.contains(['\n', '\r']) {
        return Err("This access key is not valid.".into());
    }
    let service = crate::identity::keychain_service("com.blackwall.search");
    tokio::task::spawn_blocking(move || {
        if key.trim().is_empty() {
            crate::secrets::delete(&service, ACCOUNT)
        } else {
            crate::secrets::set(&service, ACCOUNT, key.trim())
        }
    })
    .await
    .map_err(|_| "The credential store could not finish.")?
    .map_err(|e| e.explain_save("Brave's access key could not be saved."))
}
#[tauri::command]
pub async fn check_web_search(app: AppHandle) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    let key = Credentials
        .brave_key()
        .await?
        .ok_or("Save a Brave key before testing the connection.")?;
    // The owner explicitly presses a button that discloses the fixed test query and plan cost.
    BraveSearch::new(key)
        .check()
        .await
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn install_web_browser(app: AppHandle) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    PlaywrightRenderer::new(crate::identity::data_directory(&app)?.join("browser"))
        .install()
        .await
}
#[tauri::command]
pub async fn open_web_source(app: AppHandle, url: String) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    let url = reqwest::Url::parse(&url).map_err(|_| "Invalid public page URL.")?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err("Use a public HTTP/HTTPS URL without credentials.".into());
    }
    #[cfg(target_os = "macos")]
    let mut command = tokio::process::Command::new("/usr/bin/open");
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = tokio::process::Command::new("xdg-open");
    blackwall_core::apply_owner_env(&mut command, &[]);
    command
        .arg(url.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), command.status())
        .await
        .map_err(|_| "Opening the source timed out.")?
        .map_err(|_| "The source could not be opened.")?;
    if status.success() {
        Ok(())
    } else {
        Err("The source could not be opened.".into())
    }
}
