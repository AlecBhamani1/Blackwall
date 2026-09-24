//! Model and relay credentials are stored in Keychain and bound to a normalized origin.
use blackwall_core::connection::normalize_endpoint;
use reqwest::Url;

fn account(endpoint: &str) -> Result<String, String> {
    let endpoint = normalize_endpoint(endpoint).map_err(|e| e.to_string())?;
    let url = Url::parse(&endpoint).map_err(|_| "Invalid model address.")?;
    Ok(url.origin().ascii_serialization())
}

pub async fn model_key(endpoint: &str) -> Result<Option<String>, String> {
    if paired_account(endpoint).is_ok() {
        return read_pair_key("client", endpoint)
            .await?
            .map(Some)
            .ok_or_else(|| {
                "This paired computer's key is missing. Pair it again to restore access.".into()
            });
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
        #[cfg(target_os = "macos")]
        { match security_framework::passwords::generic_password(security_framework::passwords::PasswordOptions::new_generic_password(&service, &origin)) {
            Ok(value) => String::from_utf8(value).map(Some).map_err(|_| "The saved access key is not valid text.".into()),
            Err(error) if error.code() == -25300 => Ok(None),
            Err(_) => Err("Blackwall could not read the access key from Keychain. Unlock your login keychain and try again.".into()),
        } }
        #[cfg(not(target_os = "macos"))]
        { let _ = (service, origin); Ok(None) }
    }).await
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
) -> Result<(), String> {
    crate::auth::require_unlocked(&app).await?;
    if paired_account(&endpoint).is_ok() {
        return Err("Manage this connection under paired computers; its device key cannot be replaced manually.".into());
    }
    let origin = account(&endpoint)?;
    if key.len() > 8192 || key.contains(['\n', '\r']) {
        return Err("This access key is not valid.".into());
    }
    // Verify a replacement before overwriting a working Keychain entry.
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
    write_key("com.blackwall.model", origin, key).await
}
pub async fn save_relay_key(endpoint: &str, key: String) -> Result<(), String> {
    write_key("com.blackwall.relay", account(endpoint)?, key).await
}
async fn write_key(service: &'static str, origin: String, key: String) -> Result<(), String> {
    if key.len() > 8192 || key.contains(['\n', '\r']) {
        return Err("This access key is not valid.".into());
    }
    let service = crate::identity::keychain_service(service);
    tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        {
            let result = if key.trim().is_empty() {
                security_framework::passwords::delete_generic_password(&service, &origin)
            } else {
                security_framework::passwords::set_generic_password(
                    &service,
                    &origin,
                    key.trim().as_bytes(),
                )
            };
            match result {
                Ok(()) => Ok(()),
                Err(error) if key.trim().is_empty() && error.code() == -25300 => Ok(()),
                Err(_) if key.trim().is_empty() => Err(
                    "Blackwall could not remove the access key from Keychain. Check Keychain access and try again."
                        .into(),
                ),
                Err(_) => Err(
                    "Blackwall could not save the access key in Keychain. Check Keychain access and try again."
                        .into(),
                ),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (service, origin, key);
            Err("Secure access key storage currently requires macOS.".into())
        }
    })
    .await
    .map_err(|_| "Keychain access could not finish.")?
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
    write_key(kind.service(), account(&endpoint)?, String::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Opt-in because this exercises the current macOS login Keychain, not a mock.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "Creates and removes disposable native Keychain device accounts"]
    async fn native_pair_credentials_round_trip_and_removal_are_isolated() {
        use blackwall_core::pairing::secret;
        let one = format!("https://pairing-tests.invalid/s/{}/v1", secret("bws_"));
        let two = format!("https://pairing-tests.invalid/s/{}/v1", secret("bws_"));
        struct Cleanup(Vec<(&'static str, String)>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for (service, account) in &self.0 {
                    let _ =
                        security_framework::passwords::delete_generic_password(service, account);
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
        assert!(model_key(&one).await.unwrap().as_deref() == Some(&first));
        assert!(model_key(&two).await.unwrap().as_deref() == Some(&second));
        assert!(save_pair_key("client", &one, "invalid\nkey".into())
            .await
            .is_err());
        assert!(model_key(&one).await.unwrap().as_deref() == Some(&first));
        save_pair_key("client", &one, String::new()).await.unwrap();
        assert!(model_key(&one).await.is_err());
        assert!(read_pair_key("host", &one).await.unwrap().as_deref() == Some(&host));
        assert!(model_key(&two).await.unwrap().as_deref() == Some(&second));
        for (role, endpoint) in [("host", &one), ("client", &two)] {
            save_pair_key(role, endpoint, String::new()).await.unwrap();
            assert!(read_pair_key(role, endpoint).await.unwrap().is_none());
        }
    }
    #[test]
    fn two_paired_computers_on_one_relay_have_different_keychain_accounts() {
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
