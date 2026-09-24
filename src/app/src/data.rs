//! Native persistence commands; the webview cannot select arbitrary storage paths.
use blackwall_core::storage::{LocalStore, MemoryEntry, StorageError};
use serde_json::Value;
use tauri::{AppHandle, Manager};

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
                | "memoryEnabled"
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
            if profile
                .keys()
                .any(|key| !matches!(key.as_str(), "endpoint" | "name" | "model"))
            {
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
        Ok(serde_json::json!({"skills":skills,"warnings":warnings}))
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
