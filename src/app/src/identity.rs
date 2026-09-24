//! Alternate bundle identifiers isolate acceptance builds from the production app.
//! The production identifier retains its established data paths and Keychain names.
use std::{path::PathBuf, sync::OnceLock};
use tauri::{AppHandle, Manager};
const PRODUCTION_ID: &str = "com.blackwall.app";
static IDENTIFIER: OnceLock<String> = OnceLock::new();

pub fn initialize(identifier: &str) -> Result<(), String> {
    if identifier.is_empty()
        || identifier.len() > 200
        || !identifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
    {
        return Err("The application identifier is invalid.".into());
    }
    IDENTIFIER
        .set(identifier.to_owned())
        .map_err(|_| "Application identity was already initialized.".into())
}
pub fn configure_acceptance_webviews(config: &mut tauri::Config) {
    // Set through the Rust API: this Tauri version emits a Vec for the config's array field.
    // These fixed UUID bytes keep each acceptance webview persistent and independent.
    let store = match config.identifier.as_str() {
        "com.blackwall.acceptance.host" => Some([
            142, 201, 62, 100, 89, 80, 61, 128, 248, 169, 76, 147, 62, 179, 59, 191,
        ]),
        "com.blackwall.acceptance.client" => Some([
            192, 233, 139, 49, 14, 118, 71, 20, 148, 61, 96, 183, 16, 112, 245, 33,
        ]),
        _ => None,
    };
    if let Some(store) = store {
        for window in &mut config.app.windows {
            window.data_store_identifier = Some(store);
        }
    }
}
pub fn keychain_service(legacy_service: &str) -> String {
    service_for(
        IDENTIFIER
            .get()
            .map(String::as_str)
            .unwrap_or(PRODUCTION_ID),
        legacy_service,
    )
}
fn service_for(identifier: &str, legacy_service: &str) -> String {
    if identifier == PRODUCTION_ID {
        legacy_service.to_owned()
    } else {
        format!(
            "{identifier}.{}",
            legacy_service
                .strip_prefix("com.blackwall.")
                .unwrap_or(legacy_service)
        )
    }
}
pub fn data_directory(app: &AppHandle) -> Result<PathBuf, String> {
    if app.config().identifier == PRODUCTION_ID {
        app.path()
            .home_dir()
            .map(|home| home.join(".blackwall"))
            .map_err(|_| "Your home folder could not be found.".into())
    } else {
        app.path()
            .app_data_dir()
            .map_err(|_| "The application's data folder could not be found.".into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn production_services_are_preserved_and_variants_cannot_share_credentials() {
        for service in [
            "com.blackwall.lock",
            "com.blackwall.model",
            "com.blackwall.relay",
            "com.blackwall.pairing.host",
            "com.blackwall.pairing.client",
        ] {
            assert_eq!(service_for(PRODUCTION_ID, service), service);
            let host = service_for("com.blackwall.acceptance.host", service);
            let client = service_for("com.blackwall.acceptance.client", service);
            assert_ne!(host, service);
            assert_ne!(client, service);
            assert_ne!(host, client);
        }
    }
}
