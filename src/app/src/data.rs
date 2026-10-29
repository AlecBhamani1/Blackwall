//! Native persistence commands; the webview cannot select arbitrary storage paths.
use blackwall_core::storage::{LocalStore, MemoryEntry, StorageError};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

pub async fn with_store<T: Send + 'static>(
    app: &AppHandle,
    operation: impl FnOnce(&mut LocalStore) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, String> {
    crate::auth::require_unlocked(app).await?;
    store_task(app, None, operation).await
}
/// Activation and locking have a single ordering boundary. The blocking task owns
/// authorization until the actual SQLite write finishes, even if IPC is cancelled.
pub(crate) async fn activate_paired_device(
    app: &AppHandle,
    mut device: blackwall_core::pairing::PairedDevice,
) -> Result<(), String> {
    let authorization = app
        .state::<crate::auth::AuthState>()
        .authorize_commit()
        .await?;
    device.state = "active".into();
    store_task(app, Some(authorization), move |store| {
        store.save_paired_device(&device)
    })
    .await
}
async fn store_task<T: Send + 'static>(
    app: &AppHandle,
    authorization: Option<crate::auth::CommitAuthorization>,
    operation: impl FnOnce(&mut LocalStore) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, String> {
    let directory = crate::identity::data_directory(app)?;
    run_store_task(directory, authorization, operation).await
}
async fn run_store_task<T: Send + 'static>(
    directory: std::path::PathBuf,
    authorization: Option<crate::auth::CommitAuthorization>,
    operation: impl FnOnce(&mut LocalStore) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(move || {
        let result = operation(&mut LocalStore::open(&directory)?);
        drop(authorization);
        result
    })
    .await
    .map_err(|_| "The local storage task could not finish.")?
    .map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn list_sessions(app: AppHandle) -> Result<Vec<Value>, String> {
    with_store(&app, |store| store.list_sessions()).await
}
#[tauri::command]
pub async fn search_sessions(
    app: AppHandle,
    query: blackwall_core::storage::RecallQuery,
) -> Result<blackwall_core::storage::RecallResults, String> {
    let results = with_store(&app, move |store| {
        store.search_messages(blackwall_core::memory::MemoryClient::Desktop, &query)
    })
    .await?;
    crate::auth::require_unlocked(&app).await?;
    Ok(results)
}
#[tauri::command]
pub async fn load_session(app: AppHandle, id: String) -> Result<Option<Value>, String> {
    with_store(&app, move |store| store.load_session(&id)).await
}
#[tauri::command]
pub async fn save_session(app: AppHandle, session: Value) -> Result<(), String> {
    with_store(&app, move |store| store.save_session(&session)).await
}
#[tauri::command]
pub async fn delete_session(app: AppHandle, id: String) -> Result<(), String> {
    with_store(&app, move |store| store.delete_session(&id)).await
}
#[tauri::command]
pub async fn migrate_sessions(app: AppHandle, sessions: Vec<Value>) -> Result<(), String> {
    with_store(&app, move |store| store.migrate_sessions(&sessions)).await
}
#[tauri::command]
pub async fn list_memories(app: AppHandle, query: String) -> Result<Vec<MemoryEntry>, String> {
    with_store(&app, move |store| store.memories(&query)).await
}
#[tauri::command]
pub async fn save_memory(app: AppHandle, entry: MemoryEntry) -> Result<(), String> {
    with_store(&app, move |store| store.save_memory(&entry)).await
}
#[tauri::command]
pub async fn delete_memory(app: AppHandle, id: String) -> Result<(), String> {
    with_store(&app, move |store| store.delete_memory(&id)).await
}

#[tauri::command]
pub async fn list_memory_proposals(
    app: AppHandle,
) -> Result<Vec<blackwall_core::storage::MemoryProposal>, String> {
    with_store(&app, |store| store.memory_proposals()).await
}
#[tauri::command]
pub async fn approve_memory_proposal(
    app: AppHandle,
    id: String,
    content: Option<String>,
) -> Result<(), String> {
    with_store(&app, move |store| {
        store.approve_memory(&id, content.as_deref())
    })
    .await
}
#[tauri::command]
pub async fn reject_memory_proposal(app: AppHandle, id: String) -> Result<(), String> {
    with_store(&app, move |store| store.reject_memory(&id)).await
}

/// Desktop trace settings: on unless turned off, with prompts and outputs off unless turned on.
pub(crate) async fn trace_options(
    directory: std::path::PathBuf,
) -> blackwall_core::trace::TraceOptions {
    let saved = tokio::task::spawn_blocking(move || {
        LocalStore::open(&directory)
            .ok()?
            .setting("preferences")
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    blackwall_core::trace::TraceOptions {
        enabled: saved["tracesEnabled"].as_bool().unwrap_or(true),
        content: saved["traceContent"].as_bool().unwrap_or(false),
    }
}

/// The desktop's telemetry, started on first use and flushed at exit.
#[derive(Default)]
pub(crate) struct TelemetryState(std::sync::OnceLock<blackwall_core::telemetry::Telemetry>);

impl TelemetryState {
    /// Flush and stop the providers when the app exits.
    pub(crate) fn shutdown(&self) {
        if let Some(telemetry) = self.0.get() {
            telemetry.clone().shutdown();
        }
    }
}

/// A run's telemetry under the owner's current trace settings. `None` only when the data folder
/// cannot be found; with traces off the scope records nothing. A compaction request is not a run:
/// its scope only meters the summary calls. `labels` are the workspace (Agent only) and the
/// conversation ID.
pub(crate) async fn run_scope(
    app: &AppHandle,
    request_id: &str,
    mode: blackwall_core::telemetry::Mode,
    compact: bool,
    labels: (Option<&std::path::Path>, Option<&str>),
) -> Option<blackwall_core::telemetry::RunScope> {
    let directory = crate::identity::data_directory(app).ok()?;
    let options = trace_options(directory.clone()).await;
    let state = app.state::<TelemetryState>();
    let telemetry = state.0.get_or_init(|| {
        blackwall_core::telemetry::Telemetry::start(
            &directory,
            blackwall_core::telemetry::TelemetryOptions::from_trace(
                options,
                blackwall_core::telemetry::Surface::Desktop,
            ),
        )
    });
    telemetry.set_options(options);
    let scope = if compact {
        telemetry.compaction(mode)
    } else {
        telemetry.run(request_id, mode)
    };
    Some(scope.labelled(labels.0, labels.1))
}

#[tauri::command]
pub async fn load_preferences(app: AppHandle) -> Result<Value, String> {
    with_store(&app, |store| {
        Ok(store
            .setting("preferences")?
            .map(|text| serde_json::from_str(&text))
            .transpose()?
            .unwrap_or_else(|| serde_json::json!({})))
    })
    .await
}
#[tauri::command]
pub async fn save_preferences(app: AppHandle, preferences: Value) -> Result<(), String> {
    // Only accept known non-secret values. Unknown keys cannot smuggle credentials into config.
    let object = preferences.as_object().ok_or("Invalid settings.")?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "endpoint"
                | "model"
                | "connectionName"
                | "relayUrl"
                | "contextWindow"
                | "outputTokens"
                | "autoCompact"
                | "maxIterations"
                | "commandTimeoutSeconds"
                | "memoryEnabled"
                | "recallEnabled"
                | "tracesEnabled"
                | "traceContent"
                | "workspace"
                | "connections"
        )
    }) {
        return Err("Unknown setting.".into());
    }
    if let Some(endpoint) = object.get("endpoint") {
        blackwall_core::connection::normalize_endpoint(
            endpoint.as_str().ok_or("Invalid model address.")?,
        )
        .map_err(|e| e.to_string())?;
    }
    if let Some(window) = object.get("contextWindow") {
        if !window
            .as_u64()
            .is_some_and(|n| (2048..=1_000_000).contains(&n))
        {
            return Err("Choose a context window between 2,048 and 1,000,000 tokens.".into());
        }
    }
    if object
        .get("autoCompact")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("Invalid automatic compaction setting.".into());
    }
    if object
        .get("outputTokens")
        .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=32768).contains(&n)))
    {
        return Err("Choose an output reserve between 1 and 32,768 tokens.".into());
    }
    if object
        .get("maxIterations")
        .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=100).contains(&n)))
    {
        return Err("Choose an iteration limit between 1 and 100.".into());
    }
    if object
        .get("commandTimeoutSeconds")
        .is_some_and(|v| !v.as_u64().is_some_and(|n| (10..=3600).contains(&n)))
    {
        return Err("Choose a command time limit between 10 and 3,600 seconds.".into());
    }
    if object
        .values()
        .any(|v| v.as_str().is_some_and(|s| s.len() > 2048))
    {
        return Err("This setting is too long.".into());
    }
    if object
        .get("memoryEnabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("Invalid memory setting.".into());
    }
    if object
        .get("recallEnabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("Invalid recall setting.".into());
    }
    if ["tracesEnabled", "traceContent"]
        .iter()
        .any(|key| object.get(*key).is_some_and(|value| !value.is_boolean()))
    {
        return Err("Invalid trace setting.".into());
    }
    for key in ["model", "connectionName", "relayUrl", "workspace"] {
        if object.get(key).is_some_and(|value| !value.is_string()) {
            return Err("Invalid text setting.".into());
        }
    }
    if let Some(connections) = object.get("connections") {
        let profiles = connections
            .as_array()
            .filter(|profiles| profiles.len() <= 20)
            .ok_or("Save at most 20 connections.")?;
        for profile in profiles {
            let profile = profile.as_object().ok_or("Invalid saved connection.")?;
            if profile.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "endpoint" | "name" | "model" | "textToolCalls"
                )
            }) {
                return Err("Invalid saved connection field.".into());
            }
            let endpoint = profile
                .get("endpoint")
                .and_then(Value::as_str)
                .ok_or("Missing connection address.")?;
            blackwall_core::connection::normalize_endpoint(endpoint)
                .map_err(|error| error.to_string())?;
            if !profile
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.trim().is_empty() && name.len() <= 120)
            {
                return Err("Name your connection using at most 120 bytes.".into());
            }
            if profile
                .get("model")
                .is_some_and(|model| !model.as_str().is_some_and(|model| model.len() <= 2048))
            {
                return Err("Invalid saved model.".into());
            }
            if profile
                .get("textToolCalls")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err("Invalid text tool call setting.".into());
            }
        }
    }
    with_store(&app, move |store| store.patch_preferences(&preferences)).await
}

pub async fn with_skills<T: Send + 'static>(
    app: &AppHandle,
    operation: impl FnOnce(blackwall_core::skills::SkillStore) -> Result<T, blackwall_core::skills::SkillError>
        + Send
        + 'static,
) -> Result<T, String> {
    crate::auth::require_unlocked(app).await?;
    let directory = crate::identity::data_directory(app)?.join("skills");
    tokio::task::spawn_blocking(move || {
        let store = blackwall_core::skills::SkillStore::open(&directory)?;
        store.seed()?;
        operation(store)
    })
    .await
    .map_err(|_| "The skill task could not finish.")?
    .map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn list_skills(app: AppHandle) -> Result<serde_json::Value, String> {
    with_skills(&app, |store| {
        let (skills, warnings) = store.list()?;
        // Scanned on every listing, so a badge follows edits made here or on disk.
        let scans: serde_json::Map<_, _> = skills
            .iter()
            .map(|skill| {
                (
                    skill.name.clone(),
                    serde_json::json!(store.scan(&skill.name)),
                )
            })
            .collect();
        Ok(serde_json::json!({"skills":skills,"warnings":warnings,"scans":scans}))
    })
    .await
}
#[tauri::command]
pub async fn save_skill(
    app: AppHandle,
    skill: blackwall_core::skills::Skill,
) -> Result<(), String> {
    with_skills(&app, move |store| store.save(&skill)).await
}
#[tauri::command]
pub async fn delete_skill(app: AppHandle, name: String) -> Result<(), String> {
    with_skills(&app, move |store| store.remove(&name)).await
}
#[tauri::command]
pub async fn list_skill_proposals(app: AppHandle) -> Result<serde_json::Value, String> {
    with_skills(&app, |store| {
        let (proposals, warnings) = store.proposals()?;
        let scans: serde_json::Map<_, _> = proposals
            .iter()
            .map(|proposal| (proposal.id.clone(), serde_json::json!(proposal.scan())))
            .collect();
        Ok(serde_json::json!({"proposals":proposals,"warnings":warnings,"scans":scans}))
    })
    .await
}
/// Approve a proposal, with the owner's edited description and instructions when given.
#[tauri::command]
pub async fn approve_skill_proposal(
    app: AppHandle,
    id: String,
    description: String,
    body: String,
) -> Result<(), String> {
    with_skills(&app, move |store| {
        store
            .approve_edited(&id, Some((description, body)))
            .map(|_| ())
    })
    .await
}
#[tauri::command]
pub async fn reject_skill_proposal(app: AppHandle, id: String) -> Result<(), String> {
    with_skills(&app, move |store| store.reject(&id)).await
}

/// The 50 newest agent-run traces. Each file is parsed, so the list stays short.
#[tauri::command]
pub async fn list_traces(
    app: AppHandle,
) -> Result<Vec<blackwall_core::trace::TraceSummary>, String> {
    crate::auth::require_unlocked(&app).await?;
    let directory = crate::identity::data_directory(&app)?;
    tokio::task::spawn_blocking(move || blackwall_core::trace::list(&directory, 50))
        .await
        .map_err(|_| "Traces could not be read.".into())
}
#[tauri::command]
pub async fn read_trace(
    app: AppHandle,
    id: String,
) -> Result<Vec<blackwall_core::trace::SpanView>, String> {
    crate::auth::require_unlocked(&app).await?;
    let directory = crate::identity::data_directory(&app)?;
    tokio::task::spawn_blocking(move || blackwall_core::trace::read(&directory, &id))
        .await
        .map_err(|_| "This trace could not be read.")?
}

/// The Usage page: the last `days` days (1–400) of local usage metrics, ending today in the
/// viewer's zone, `offset_minutes` east of UTC (the webview's `-getTimezoneOffset()`).
#[tauri::command]
pub async fn usage_summary(
    app: AppHandle,
    days: u16,
    offset_minutes: i32,
) -> Result<blackwall_core::usage::UsageSummary, String> {
    crate::auth::require_unlocked(&app).await?;
    let directory = crate::identity::data_directory(&app)?;
    let enabled = trace_options(directory.clone()).await.enabled;
    let (from, to) = blackwall_core::usage::Day::range(
        days.clamp(1, 400),
        blackwall_core::usage::Day::today(offset_minutes),
    );
    let mut summary = tokio::task::spawn_blocking(move || {
        blackwall_core::usage::summary(&directory, from, to, offset_minutes)
    })
    .await
    .map_err(|_| "Usage could not be read.")?;
    summary.telemetry_enabled = enabled;
    Ok(summary)
}
/// Export the last `days` local days of metrics and traces into a folder the owner picks. The folder
/// comes from the native picker, never from the webview. `None` when the picker is cancelled.
#[tauri::command]
pub async fn export_telemetry(
    app: AppHandle,
    days: u16,
    offset_minutes: i32,
) -> Result<Option<blackwall_core::usage::ExportReport>, String> {
    crate::auth::require_unlocked(&app).await?;
    let directory = crate::identity::data_directory(&app)?;
    let picker = app.clone();
    let chosen = tokio::task::spawn_blocking(move || {
        picker
            .dialog()
            .file()
            .set_title("Export usage telemetry")
            .blocking_pick_folder()
    })
    .await
    .map_err(|_| "The picker could not open.")?;
    let Some(chosen) = chosen else {
        return Ok(None);
    };
    let dest = chosen.into_path().map_err(|_| "Choose a local folder.")?;
    crate::auth::require_unlocked(&app).await?;
    let (from, to) = blackwall_core::usage::Day::range(
        days.clamp(1, 400),
        blackwall_core::usage::Day::today(offset_minutes),
    );
    tokio::task::spawn_blocking(move || {
        blackwall_core::usage::export(&directory, from, to, offset_minutes, &dest)
    })
    .await
    .map_err(|_| "The export could not finish.")?
    .map(Some)
    .map_err(|error| error.to_string())
}

/// Skills found in other harnesses' folders (Claude Code, Codex, Agent Skills), ready to review.
#[tauri::command]
pub async fn find_skill_imports(
    app: AppHandle,
) -> Result<Vec<blackwall_core::skill_import::ImportCandidate>, String> {
    let home = app
        .path()
        .home_dir()
        .map_err(|_| "Your home folder could not be found.")?;
    with_skills(&app, move |store| {
        let mut found: Vec<_> = blackwall_core::skill_import::known_folders(&home)
            .into_iter()
            .flat_map(|(origin, folder)| blackwall_core::skill_import::scan(origin, &folder))
            .collect();
        blackwall_core::skill_import::mark(&store, &mut found);
        Ok(found)
    })
    .await
}
/// Pick a skill file or folder to review for import. `None` when the picker is cancelled.
#[tauri::command]
pub async fn choose_skill_import(
    app: AppHandle,
    folder: bool,
) -> Result<Option<Vec<blackwall_core::skill_import::ImportCandidate>>, String> {
    crate::auth::require_unlocked(&app).await?;
    let picker = app.clone();
    let chosen = tokio::task::spawn_blocking(move || {
        let dialog = picker.dialog().file().set_title("Choose skills to import");
        if folder {
            dialog.blocking_pick_folder()
        } else {
            dialog.add_filter("Skill", &["md"]).blocking_pick_file()
        }
    })
    .await
    .map_err(|_| "The picker could not open.")?;
    let Some(chosen) = chosen else {
        return Ok(None);
    };
    let path = chosen
        .into_path()
        .map_err(|_| "Choose a local file or folder.")?;
    let origin = if folder {
        "Chosen folder"
    } else {
        "Chosen file"
    };
    with_skills(&app, move |store| {
        let mut found = blackwall_core::skill_import::scan(origin, &path);
        blackwall_core::skill_import::mark(&store, &mut found);
        Ok(Some(found))
    })
    .await
}
/// Import one skill found by `find_skill_imports`, `choose_skill_import`, or
/// `get_suggested_skill`. `allow` is the owner's choice to import despite scan findings.
#[tauri::command]
pub async fn import_skill(
    app: AppHandle,
    source: String,
    replace: bool,
    allow: bool,
) -> Result<blackwall_core::skills::Skill, String> {
    let path = std::path::PathBuf::from(&source);
    let skill = with_skills(&app, move |store| {
        blackwall_core::skill_import::import(&store, &path, replace, allow)
    })
    .await?;
    blackwall_core::skill_suggest::discard(std::path::Path::new(&source));
    Ok(skill)
}
/// Suggested skills from the official repositories, marking ones already saved.
#[tauri::command]
pub async fn list_suggested_skills(app: AppHandle) -> Result<serde_json::Value, String> {
    with_skills(&app, |store| {
        Ok(serde_json::json!(blackwall_core::skill_suggest::SUGGESTED
            .iter()
            .map(|suggested| {
                let mut entry = serde_json::json!(suggested);
                entry["saved"] = store.contains(suggested.name).into();
                entry
            })
            .collect::<Vec<_>>()))
    })
    .await
}
/// Download a suggested skill at its reviewed version and scan it for review. Nothing is saved
/// until `import_skill`.
#[tauri::command]
pub async fn get_suggested_skill(
    app: AppHandle,
    name: String,
) -> Result<blackwall_core::skill_import::ImportCandidate, String> {
    crate::auth::require_unlocked(&app).await?;
    let suggested = blackwall_core::skill_suggest::find(&name).ok_or("Unknown suggested skill.")?;
    let source = blackwall_core::skill_suggest::download(suggested, &std::env::temp_dir()).await?;
    let origin = suggested.repository;
    let scanned = source.clone();
    let found = with_skills(&app, move |store| {
        let mut found = blackwall_core::skill_import::scan(origin, &scanned);
        blackwall_core::skill_import::mark(&store, &mut found);
        Ok(found)
    })
    .await;
    match found.map(|mut found| found.pop()) {
        Ok(Some(candidate)) => Ok(candidate),
        Ok(None) => {
            blackwall_core::skill_suggest::discard(&source);
            Err("The downloaded skill could not be read.".into())
        }
        Err(error) => {
            blackwall_core::skill_suggest::discard(&source);
            Err(error)
        }
    }
}
/// Remove downloads that were reviewed but not imported.
#[tauri::command]
pub async fn discard_skill_downloads(sources: Vec<String>) {
    let _ = tokio::task::spawn_blocking(move || {
        for source in sources {
            blackwall_core::skill_suggest::discard(std::path::Path::new(&source));
        }
    })
    .await;
}

/// MCP edits share the CLI's validation; secret values go only to the credential store.
/// Like saved model keys, the lock is checked first and not held, so a credential store prompt
/// cannot stall the rest of the app.
async fn with_mcp<T: Send + 'static>(
    app: &AppHandle,
    operation: impl FnOnce(&std::path::Path) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    crate::auth::require_unlocked(app).await?;
    let directory = crate::identity::data_directory(app)?;
    tokio::task::spawn_blocking(move || operation(&directory))
        .await
        .map_err(|_| "The MCP task could not finish.")?
}
#[tauri::command]
pub async fn list_mcp_servers(app: AppHandle) -> Result<serde_json::Value, String> {
    with_mcp(&app, |directory| {
        Ok(serde_json::json!(blackwall_core::mcp::read_config(
            directory
        )?))
    })
    .await
}
#[tauri::command]
pub async fn add_mcp_server(app: AppHandle, name: String, command: String) -> Result<(), String> {
    let command = command.split_whitespace().map(str::to_owned).collect();
    with_mcp(&app, move |directory| {
        blackwall_core::mcp::add_server(directory, &name, command).map(|_| ())
    })
    .await
}
#[tauri::command]
pub async fn set_mcp_server_enabled(
    app: AppHandle,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    with_mcp(&app, move |directory| {
        blackwall_core::mcp::set_enabled(directory, &name, enabled)
    })
    .await
}
#[tauri::command]
pub async fn remove_mcp_server(app: AppHandle, name: String) -> Result<(), String> {
    with_mcp(&app, move |directory| {
        blackwall_core::mcp::remove_server(directory, &name)
    })
    .await
}
#[tauri::command]
pub async fn save_mcp_secret(
    app: AppHandle,
    name: String,
    key: String,
    value: String,
) -> Result<(), String> {
    with_mcp(&app, move |directory| {
        blackwall_core::mcp::save_secret(directory, &name, &key, &value)
    })
    .await
}
#[tauri::command]
pub async fn list_mcp_tool_rules(
    app: AppHandle,
) -> Result<Vec<blackwall_core::policy::ToolRule>, String> {
    with_mcp(&app, |directory| {
        blackwall_core::policy::tool_rules(directory)
    })
    .await
}
#[tauri::command]
pub async fn remove_mcp_tool_rule(
    app: AppHandle,
    rule: blackwall_core::policy::ToolRule,
) -> Result<(), String> {
    with_mcp(&app, move |directory| {
        blackwall_core::policy::remove_tool_rule(directory, &rule)
    })
    .await
}
#[tauri::command]
pub async fn delete_mcp_secret(app: AppHandle, name: String, key: String) -> Result<(), String> {
    with_mcp(&app, move |directory| {
        blackwall_core::mcp::delete_secret(directory, &name, &key)
    })
    .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod pairing_commit_tests {
    use super::*;
    use blackwall_core::pairing::*;
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn directory() -> Directory {
        Directory(std::env::temp_dir().join(format!("blackwall-commit-test-{}", secret("bwd_"))))
    }
    #[tokio::test]
    async fn cancelled_caller_keeps_authorization_until_the_real_database_worker_finishes() {
        let directory = directory();
        let auth = crate::auth::AuthState::unlocked_for_test();
        let authorization = auth.authorize_commit().await.unwrap();
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (resume, gate) = std::sync::mpsc::channel();
        let path = directory.0.clone();
        let (salt, hash) = digest(&secret("bw1_"));
        let device = PairedDevice {
            id: secret("bws_"),
            role: "host".into(),
            name: "Test".into(),
            relay_url: "https://pairing-tests.invalid".into(),
            model: "test".into(),
            upstream: Some("http://127.0.0.1:11434/v1".into()),
            salt,
            hash,
            state: "active".into(),
        };
        let caller = tokio::spawn(run_store_task(path, Some(authorization), move |store| {
            let _ = entered.send(());
            gate.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            store.save_paired_device(&device)
        }));
        wait.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let locking = auth.clone();
        let mut lock = tokio::spawn(async move { locking.begin_lock().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut lock)
                .await
                .is_err()
        );
        resume.send(()).unwrap();
        assert!(lock.await.unwrap().is_ok());
        assert_eq!(
            LocalStore::open(&directory.0)
                .unwrap()
                .paired_devices()
                .unwrap()[0]
                .state,
            "active"
        );
        assert!(auth.ensure_unlocked().await.is_err());
    }
    #[tokio::test]
    async fn a_real_database_open_failure_releases_the_commit_guard_without_activation() {
        let directory = directory();
        std::fs::create_dir_all(directory.0.join("blackwall.sqlite3")).unwrap();
        let auth = crate::auth::AuthState::unlocked_for_test();
        let authorization = auth.authorize_commit().await.unwrap();
        let result = run_store_task(directory.0.clone(), Some(authorization), |_| {
            panic!("A failed database open must not execute its write")
        })
        .await;
        let result: Result<(), String> = result;
        assert!(result.is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), auth.begin_lock())
                .await
                .unwrap()
                .is_ok()
        );
    }
}
