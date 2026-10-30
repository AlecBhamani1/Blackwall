//! Alternate bundle identifiers isolate acceptance builds from the production app.
//! The production identifier retains its established data paths and credential-store names.
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::OnceLock,
};
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
    let override_path = std::env::var_os("BLACKWALL_DATA_DIR");
    data_directory_for(
        &app.config().identifier,
        override_path.as_deref(),
        app.path().home_dir().ok().as_deref(),
        app.path().app_data_dir().ok().as_deref(),
    )
}
fn data_directory_for(
    identifier: &str,
    override_path: Option<&OsStr>,
    home: Option<&Path>,
    app_data: Option<&Path>,
) -> Result<PathBuf, String> {
    if identifier == PRODUCTION_ID {
        blackwall_core::data_directory::resolve(override_path, home).map_err(str::to_owned)
    } else {
        app_data
            .map(Path::to_path_buf)
            .ok_or_else(|| "The application's data folder could not be found.".into())
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

    #[test]
    fn production_data_uses_the_shared_override_and_default_rules() {
        let home = Path::new("/Users/owner");
        let app_data = Path::new("/acceptance/data");
        assert_eq!(
            data_directory_for(
                PRODUCTION_ID,
                Some(OsStr::new("/var/blackwall-owner")),
                Some(home),
                Some(app_data),
            )
            .unwrap(),
            Path::new("/var/blackwall-owner")
        );
        assert_eq!(
            data_directory_for(PRODUCTION_ID, None, Some(home), Some(app_data)).unwrap(),
            home.join(".blackwall")
        );
        assert_eq!(
            data_directory_for(
                PRODUCTION_ID,
                Some(OsStr::new("relative")),
                Some(home),
                Some(app_data),
            ),
            blackwall_core::data_directory::resolve(Some(OsStr::new("relative")), Some(home))
                .map_err(str::to_owned)
        );
    }

    #[test]
    fn acceptance_identity_keeps_its_isolated_data_directory() {
        let app_data = Path::new("/acceptance/data");
        assert_eq!(
            data_directory_for(
                "com.blackwall.acceptance.host",
                Some(OsStr::new("/var/production-data")),
                None,
                Some(app_data),
            )
            .unwrap(),
            app_data
        );
    }
}
