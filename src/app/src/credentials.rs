//! Model and relay credentials are stored in the platform credential store and bound to a
//! normalized origin.
use blackwall_core::connection::normalize_endpoint;
use reqwest::Url;
use tauri::Manager;

fn account(endpoint: &str) -> Result<String, String> {
    let endpoint = normalize_endpoint(endpoint).map_err(|e| e.to_string())?;
    let url = Url::parse(&endpoint).map_err(|_| "Invalid model address.")?;
    Ok(url.origin().ascii_serialization())
}

type SessionKeys = std::sync::Mutex<std::collections::HashMap<String, String>>;
fn sessions(
    keys: &SessionKeys,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, String>> {
    keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `saved` is false when the key is held for this session only.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct SavedKey {
    pub saved: bool,
}

pub async fn model_key(
    state: &crate::commands::AppState,
    endpoint: &str,
) -> Result<Option<String>, String> {
    if paired_account(endpoint).is_ok() {
        return read_pair_key("client", endpoint)
            .await?
            .map(Some)
            .ok_or_else(|| {
                "This paired computer's key is missing. Pair it again to restore access.".into()
            });
    }
    if let Some(key) = sessions(&state.session_keys).get(&account(endpoint)?) {
        return Ok(Some(key.clone()));
    }
    read_key("com.blackwall.model", endpoint).await
}
pub async fn relay_key(endpoint: &str) -> Result<Option<String>, String> {
    read_key("com.blackwall.relay", endpoint).await
}
async fn read_key(service: &'static str, endpoint: &str) -> Result<Option<String>, String> {
    read_secret(service, account(endpoint)?).await
}
async fn read_secret(service: &'static str, origin: String) -> Result<Option<String>, String> {
    let service = crate::identity::keychain_service(service);
    crate::keychain::read(move || {
        crate::secrets::get(&service, &origin).map_err(|error| {
            error.explain(format!(
                "Blackwall could not read the access key from {}. {}, then try again.",
                crate::secrets::STORE,
                crate::secrets::RECOVER
            ))
        })
    })
    .await
}
fn paired_account(endpoint: &str) -> Result<String, String> {
    let endpoint = normalize_endpoint(endpoint).map_err(|_| "Invalid paired computer address.")?;
    let url = Url::parse(&endpoint).map_err(|_| "Invalid paired computer address.")?;
    blackwall_core::pairing::normalize_relay(&url.origin().ascii_serialization())?;
    let id = url
        .path()
        .strip_prefix("/s/")
        .and_then(|value| value.strip_suffix("/v1"))
        .ok_or("Invalid paired computer address.")?;
    if !blackwall_core::pairing::valid_secret(id, "bws_") {
        return Err("Invalid paired computer address.".into());
    }
    Ok(endpoint)
}
fn pair_service(role: &str) -> Result<&'static str, String> {
    match role {
        "host" => Ok("com.blackwall.pairing.host"),
        "client" => Ok("com.blackwall.pairing.client"),
        _ => Err("Invalid pairing role.".into()),
    }
}
pub async fn read_pair_key(role: &str, endpoint: &str) -> Result<Option<String>, String> {
    read_secret(pair_service(role)?, paired_account(endpoint)?).await
}
pub async fn save_pair_key(role: &str, endpoint: &str, key: String) -> Result<(), String> {
    write_key(pair_service(role)?, paired_account(endpoint)?, key).await
}

#[tauri::command]
pub async fn save_model_key(
    app: tauri::AppHandle,
    endpoint: String,
    key: String,
) -> Result<SavedKey, String> {
    crate::auth::require_unlocked(&app).await?;
    if paired_account(&endpoint).is_ok() {
        return Err("Manage this connection under paired computers; its device key cannot be replaced manually.".into());
    }
    let origin = account(&endpoint)?;
    if key.len() > 8192 || key.contains(['\n', '\r']) {
        return Err("This access key is not valid.".into());
    }
    // Verify a replacement before overwriting a working saved entry.
    if !key.trim().is_empty() {
        let endpoint = normalize_endpoint(&endpoint).map_err(|error| error.to_string())?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|_| "The connection test could not start.")?;
        let response = client
            .get(format!("{endpoint}/models"))
            .bearer_auth(key.trim())
            .send()
            .await
            .map_err(|_| {
                "The service could not be reached. Your previous access key has been kept."
            })?;
        if !response.status().is_success() {
            return Err(
                "The service did not accept this access key. Your previous key has been kept."
                    .into(),
            );
        }
        crate::auth::require_unlocked(&app).await?;
    }
    let state = app.state::<crate::commands::AppState>();
    commit_model_key(&state.session_keys, origin, key, crate::secrets::set).await
}
/// Saves in the keyring; with none, keeps the key in memory for this run only.
async fn commit_model_key(
    session_keys: &SessionKeys,
    origin: String,
    key: String,
    set: blackwall_core::secrets::SetSecret,
) -> Result<SavedKey, String> {
    match write_key_with(set, "com.blackwall.model", origin.clone(), key.clone()).await {
        Ok(()) => {
            sessions(session_keys).remove(&origin);
            Ok(SavedKey { saved: true })
        }
        Err(KeyFailure::NoKeyring) => {
            let mut held = sessions(session_keys);
            if key.trim().is_empty() {
                held.remove(&origin);
            } else {
                held.insert(origin, key.trim().to_owned());
            }
            Ok(SavedKey { saved: false })
        }
        Err(KeyFailure::Message(message)) => Err(message),
    }
}
pub async fn save_relay_key(endpoint: &str, key: String) -> Result<(), String> {
    write_key("com.blackwall.relay", account(endpoint)?, key).await
}
async fn write_key(service: &'static str, origin: String, key: String) -> Result<(), String> {
    write_key_with(crate::secrets::set, service, origin, key)
        .await
        .map_err(|failure| match failure {
            KeyFailure::NoKeyring => crate::secrets::SecretError::Unavailable.explain(""),
            KeyFailure::Message(message) => message,
        })
}
enum KeyFailure {
    NoKeyring,
    Message(String),
}
async fn write_key_with(
    set: blackwall_core::secrets::SetSecret,
    service: &'static str,
    origin: String,
    key: String,
) -> Result<(), KeyFailure> {
    if key.len() > 8192 || key.contains(['\n', '\r']) {
        return Err(KeyFailure::Message("This access key is not valid.".into()));
    }
    let service = crate::identity::keychain_service(service);
    tokio::task::spawn_blocking(move || {
        let (store, recover) = (crate::secrets::STORE, crate::secrets::RECOVER);
        let (result, failure) = if key.trim().is_empty() {
            (
                crate::secrets::delete(&service, &origin),
                format!("Blackwall could not remove the access key from {store}. {recover}, then try again."),
            )
        } else {
            (
                set(&service, &origin, key.trim()),
                format!("Blackwall could not save the access key in {store}. {recover}, then try again."),
            )
        };
        result.map_err(|error| match error {
            crate::secrets::SecretError::Unavailable => KeyFailure::NoKeyring,
            other => KeyFailure::Message(other.explain(failure)),
        })
    })
    .await
    .map_err(|_| {
        KeyFailure::Message(format!("Access to {} could not finish.", crate::secrets::STORE))
    })?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialKind {
    Model,
    Relay,
}
impl CredentialKind {
    fn service(&self) -> &'static str {
        match self {
            Self::Model => "com.blackwall.model",
            Self::Relay => "com.blackwall.relay",
        }
    }
}
/// Returns presence only; saved credential values never cross the IPC boundary.
#[tauri::command]
pub async fn has_saved_credential(
    app: tauri::AppHandle,
    kind: CredentialKind,
    endpoint: String,
) -> Result<bool, String> {
    crate::auth::require_unlocked(&app).await?;
    if matches!(&kind, CredentialKind::Model) && paired_account(&endpoint).is_ok() {
        return Ok(read_pair_key("client", &endpoint).await?.is_some());
    }
    Ok(read_key(kind.service(), &endpoint).await?.is_some())
}
#[tauri::command]
pub async fn remove_saved_credential(
    app: tauri::AppHandle,
    kind: CredentialKind,
    endpoint: String,
) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    if matches!(&kind, CredentialKind::Model) && paired_account(&endpoint).is_ok() {
        return Err(
            "Remove this connection under paired computers to remove its device key.".into(),
        );
    }
    if matches!(&kind, CredentialKind::Model) {
        let state = app.state::<crate::commands::AppState>();
        let origin = account(&endpoint)?;
        return commit_model_key(
            &state.session_keys,
            origin,
            String::new(),
            crate::secrets::set,
        )
        .await
        .map(|_| ());
    }
    write_key(kind.service(), account(&endpoint)?, String::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use blackwall_core::secrets::KEYRING_OFF_NOTE;
    /// Opt-in because this exercises the signed-in user's credential store, not a mock.
    #[tokio::test]
    #[ignore = "Creates and removes disposable native credential-store device accounts"]
    async fn native_pair_credentials_round_trip_and_removal_are_isolated() {
        use blackwall_core::pairing::secret;
        let state = crate::commands::AppState::new().unwrap();
        let one = format!("https://pairing-tests.invalid/s/{}/v1", secret("bws_"));
        let two = format!("https://pairing-tests.invalid/s/{}/v1", secret("bws_"));
        struct Cleanup(Vec<(&'static str, String)>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for (service, account) in &self.0 {
                    let _ = crate::secrets::delete(service, account);
                }
            }
        }
        let _cleanup = Cleanup(vec![
            (pair_service("host").unwrap(), one.clone()),
            (pair_service("client").unwrap(), one.clone()),
            (pair_service("client").unwrap(), two.clone()),
        ]);
        let host = secret("bwh_");
        let first = secret("bw1_");
        let second = secret("bw1_");
        save_pair_key("host", &one, host.clone()).await.unwrap();
        save_pair_key("client", &one, first.clone()).await.unwrap();
        save_pair_key("client", &two, second.clone()).await.unwrap();
        // Boolean assertions avoid printing credential values on failure.
        assert!(read_pair_key("host", &one).await.unwrap().as_deref() == Some(&host));
        assert!(model_key(&state, &one).await.unwrap().as_deref() == Some(&first));
        assert!(model_key(&state, &two).await.unwrap().as_deref() == Some(&second));
        assert!(save_pair_key("client", &one, "invalid\nkey".into())
            .await
            .is_err());
        assert!(model_key(&state, &one).await.unwrap().as_deref() == Some(&first));
        save_pair_key("client", &one, String::new()).await.unwrap();
        assert!(model_key(&state, &one).await.is_err());
        assert!(read_pair_key("host", &one).await.unwrap().as_deref() == Some(&host));
        assert!(model_key(&state, &two).await.unwrap().as_deref() == Some(&second));
        for (role, endpoint) in [("host", &one), ("client", &two)] {
            save_pair_key(role, endpoint, String::new()).await.unwrap();
            assert!(read_pair_key(role, endpoint).await.unwrap().is_none());
        }
    }
    fn no_keyring(_: &str, _: &str, _: &str) -> Result<(), blackwall_core::secrets::SecretError> {
        Err(blackwall_core::secrets::SecretError::Unavailable)
    }
    #[tokio::test]
    async fn unavailable_keyring_keeps_access_key_for_session_only() {
        let state = crate::commands::AppState::new().unwrap();
        let endpoint = "https://model.example/v1";
        let saved = commit_model_key(
            &state.session_keys,
            account(endpoint).unwrap(),
            "sk-session".into(),
            no_keyring,
        )
        .await
        .unwrap();
        assert_eq!(saved, SavedKey { saved: false });
        // Boolean assertion avoids printing the key on failure.
        assert!(model_key(&state, endpoint).await.unwrap().as_deref() == Some("sk-session"));
        // The key is held per origin, so another origin does not see it.
        assert!(!state
            .session_keys
            .lock()
            .unwrap()
            .contains_key("https://other.example"));
        // Clearing the access key drops the session copy as well.
        commit_model_key(
            &state.session_keys,
            account(endpoint).unwrap(),
            String::new(),
            no_keyring,
        )
        .await
        .unwrap();
        assert!(state.session_keys.lock().unwrap().is_empty());
    }
    #[test]
    fn keyring_only_saves_fail_with_the_shared_note() {
        use blackwall_core::{command_tools, mcp, telemetry::export};
        let directory = std::env::temp_dir().join(format!(
            "blackwall-keyring-off-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(command_tools::COMMAND_TOOLS_FILE),
            r#"{"tools":[{"name":"issues","description":"Issues","kind":"http","url":"https://tools.example/issues"}]}"#,
        )
        .unwrap();
        mcp::add_server(&directory, "docs", vec!["server".into()]).unwrap();
        assert_eq!(
            export::save_header_with("https://otel.example", "x-api-key", "k", no_keyring)
                .unwrap_err()
                .to_string(),
            KEYRING_OFF_NOTE
        );
        assert_eq!(
            mcp::save_secret_with(&directory, "docs", "DOCS_TOKEN", "v", no_keyring).unwrap_err(),
            KEYRING_OFF_NOTE
        );
        assert_eq!(
            command_tools::save_secret_with(&directory, "issues", "GITLAB_TOKEN", "v", no_keyring)
                .unwrap_err(),
            KEYRING_OFF_NOTE
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
    #[test]
    fn two_paired_computers_on_one_relay_have_different_credential_accounts() {
        let first = blackwall_core::pairing::secret("bws_");
        let second = blackwall_core::pairing::secret("bws_");
        let one = format!("https://relay.example/s/{first}/v1");
        let two = format!("https://relay.example/s/{second}/v1");
        assert_ne!(paired_account(&one).unwrap(), paired_account(&two).unwrap());
        assert_ne!(
            pair_service("host").unwrap(),
            pair_service("client").unwrap()
        );
        assert!(paired_account("https://relay.example/v1").is_err());
        assert!(paired_account(&format!("{one}?key=secret")).is_err());
        assert!(paired_account(&one.replace("https:", "http:")).is_err());
    }
    #[test]
    fn credential_accounts_are_origin_scoped_and_separate_by_service() {
        assert_eq!(
            account("https://MODEL.example:443/v1").unwrap(),
            "https://model.example"
        );
        assert_eq!(
            account("https://model.example/custom/v1").unwrap(),
            account("https://model.example").unwrap()
        );
        assert_ne!(
            account("http://model.example").unwrap(),
            account("https://model.example").unwrap()
        );
        assert_ne!(
            CredentialKind::Model.service(),
            CredentialKind::Relay.service()
        );
        assert!(account("https://secret@example.com").is_err());
        assert!(account("https://example.com?token=secret").is_err());
    }
}
