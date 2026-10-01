use blackwall_core::{connection::normalize_endpoint, storage::LocalStore};
use serde_json::{json, Value};

const SETTINGS_KEY: &str = "cli-settings";
const DEFAULT_ENDPOINT: &str = "http://localhost:11434/v1";
const DEFAULT_MODEL: &str = "llama3.2";

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub endpoint: String,
    pub model: String,
    pub web_enabled: bool,
    pub memory_enabled: bool,
}

impl Settings {
    pub fn load(store: &LocalStore) -> Result<Self, String> {
        load_with_environment(store, |key| std::env::var(key).ok())
    }

    /// Check all fields before changing either normalized value.
    pub fn validate(&mut self) -> Result<(), String> {
        let endpoint = validated_endpoint(&self.endpoint)?;
        let model = validated_model(&self.model)?;
        self.endpoint = endpoint;
        self.model = model;
        Ok(())
    }

    pub fn save(&self, store: &LocalStore) -> Result<(), String> {
        let mut validated = self.clone();
        validated.validate()?;
        let body = json!({
            "endpoint": validated.endpoint,
            "model": validated.model,
            "webEnabled": validated.web_enabled,
            "memoryEnabled": validated.memory_enabled,
        });
        store
            .save_setting(SETTINGS_KEY, &body.to_string())
            .map_err(|error| error.to_string())
    }

    pub fn set(&mut self, store: &LocalStore, key: &str, value: &str) -> Result<(), String> {
        self.set_with_persistence(key, value, |updated| updated.save(store))
    }

    fn set_with_persistence(
        &mut self,
        key: &str,
        value: &str,
        persist: impl FnOnce(&Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut updated = self.clone();
        match key {
            "endpoint" => updated.endpoint = value.into(),
            "model" => updated.model = value.into(),
            "web" => updated.web_enabled = on_or_off(value)?,
            "memory" => updated.memory_enabled = on_or_off(value)?,
            _ => return Err("Choose a setting: model, endpoint, web, or memory.".into()),
        }
        updated.validate()?;
        persist(&updated)?;
        *self = updated;
        Ok(())
    }

    pub fn describe(&self) -> String {
        // Public fields also serve setup's staged edits. Never print an unvalidated
        // endpoint or terminal control characters from an unfinished edit.
        let endpoint =
            validated_endpoint(&self.endpoint).unwrap_or_else(|_| "[invalid endpoint]".into());
        let model = validated_model(&self.model).unwrap_or_else(|_| "[invalid model]".into());
        format!(
            "model: {model}\nendpoint: {endpoint}\nweb: {}\nmemory: {}",
            if self.web_enabled { "on" } else { "off" },
            if self.memory_enabled { "on" } else { "off" },
        )
    }
}

/// An injected environment lookup keeps precedence tests independent of the
/// developer's environment and avoids mutating process-wide environment variables.
fn load_with_environment(
    store: &LocalStore,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<Settings, String> {
    if let Some(body) = store
        .setting(SETTINGS_KEY)
        .map_err(|error| error.to_string())?
    {
        return saved_settings(&body);
    }

    let preferences: Value = store
        .setting("preferences")
        .map_err(|error| error.to_string())?
        .map(|body| {
            serde_json::from_str(&body)
                .map_err(|_| "The saved desktop preferences are invalid.".to_owned())
        })
        .transpose()?
        .unwrap_or_else(|| json!({}));
    let object = preferences
        .as_object()
        .ok_or("The saved desktop preferences are invalid.")?;
    let endpoint = preference_text(object.get("endpoint"), "endpoint")?
        .or_else(|| environment_value(&environment, "BLACKWALL_MODEL_ENDPOINT"))
        .or_else(|| environment_value(&environment, "OLLAMA_HOST"))
        .unwrap_or_else(|| DEFAULT_ENDPOINT.into());
    let model = preference_text(object.get("model"), "model")?
        .or_else(|| environment_value(&environment, "BLACKWALL_MODEL"))
        .unwrap_or_else(|| DEFAULT_MODEL.into());
    let memory_enabled = match object.get("memoryEnabled") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or("The saved desktop memory setting is invalid.")?,
    };
    let mut settings = Settings {
        endpoint,
        model,
        web_enabled: false,
        memory_enabled,
    };
    settings.validate()?;
    Ok(settings)
}

fn saved_settings(body: &str) -> Result<Settings, String> {
    let invalid = "The saved CLI settings are invalid.";
    let value: Value = serde_json::from_str(body).map_err(|_| invalid)?;
    let object = value.as_object().ok_or(invalid)?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "endpoint" | "model" | "webEnabled" | "memoryEnabled"
        )
    }) {
        return Err(invalid.into());
    }
    let mut settings = Settings {
        endpoint: object
            .get("endpoint")
            .and_then(Value::as_str)
            .ok_or(invalid)?
            .into(),
        model: object
            .get("model")
            .and_then(Value::as_str)
            .ok_or(invalid)?
            .into(),
        web_enabled: object
            .get("webEnabled")
            .and_then(Value::as_bool)
            .ok_or(invalid)?,
        memory_enabled: object
            .get("memoryEnabled")
            .and_then(Value::as_bool)
            .ok_or(invalid)?,
    };
    settings
        .validate()
        .map_err(|error| format!("The saved CLI settings are invalid. {error}"))?;
    Ok(settings)
}

fn preference_text(value: Option<&Value>, key: &str) -> Result<Option<String>, String> {
    value
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("The saved desktop {key} setting is invalid."))
        })
        .transpose()
}

fn environment_value(environment: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    environment(key).filter(|value| !value.trim().is_empty())
}

fn validated_endpoint(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err("Enter a model endpoint without control characters.".into());
    }
    normalize_endpoint(value).map_err(|error| error.to_string())
}

fn validated_model(value: &str) -> Result<String, String> {
    let model = value.trim();
    if model.is_empty() || model.len() > 256 || value.chars().any(char::is_control) {
        return Err("Choose a model id of 1 to 256 bytes without control characters.".into());
    }
    Ok(model.into())
}

fn on_or_off(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("Choose on or off.".into()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            Self(std::env::temp_dir().join(format!(
                "blackwall-cli-settings-{}-{}-{}",
                std::process::id(),
                unique.as_nanos(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed),
            )))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn defaults(store: &LocalStore) -> Settings {
        load_with_environment(store, |_| None).unwrap()
    }

    #[test]
    fn absent_settings_use_offline_defaults() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        assert_eq!(
            defaults(&store),
            Settings {
                endpoint: DEFAULT_ENDPOINT.into(),
                model: DEFAULT_MODEL.into(),
                web_enabled: false,
                memory_enabled: false,
            }
        );
        assert!(store.setting(SETTINGS_KEY).unwrap().is_none());
    }

    #[test]
    fn desktop_preferences_precede_environment_and_cli_preferences_precede_both() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let desktop = json!({
            "endpoint": "desktop.example:11434",
            "model": "desktop-model",
            "memoryEnabled": true,
            "contextWindow": 32000,
        })
        .to_string();
        store.save_setting("preferences", &desktop).unwrap();
        let environment = |key: &str| match key {
            "BLACKWALL_MODEL_ENDPOINT" => Some("http://environment.example/v1".into()),
            "BLACKWALL_MODEL" => Some("environment-model".into()),
            _ => None,
        };
        let mut settings = load_with_environment(&store, environment).unwrap();
        assert_eq!(settings.endpoint, "http://desktop.example:11434/v1");
        assert_eq!(settings.model, "desktop-model");
        assert!(settings.memory_enabled);
        assert!(!settings.web_enabled);
        settings
            .set(&store, "endpoint", "https://cli.example/")
            .unwrap();
        settings.set(&store, "model", "cli-model").unwrap();
        settings.set(&store, "web", "on").unwrap();
        settings.set(&store, "memory", "off").unwrap();
        assert_eq!(
            load_with_environment(&store, environment).unwrap(),
            settings
        );
        assert_eq!(store.setting("preferences").unwrap(), Some(desktop));
        // Already-saved CLI settings remain usable even if an unused fallback is corrupt.
        store.save_setting("preferences", "broken").unwrap();
        assert_eq!(
            load_with_environment(&store, environment).unwrap(),
            settings
        );
    }

    #[test]
    fn endpoint_environment_precedence_ignores_empty_values() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let settings = load_with_environment(&store, |key| match key {
            "BLACKWALL_MODEL_ENDPOINT" => Some("  ".into()),
            "OLLAMA_HOST" => Some("environment.example:11434".into()),
            "BLACKWALL_MODEL" => Some(" environment-model ".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(settings.endpoint, "http://environment.example:11434/v1");
        assert_eq!(settings.model, "environment-model");
        let preferred = load_with_environment(&store, |key| match key {
            "BLACKWALL_MODEL_ENDPOINT" => Some("https://preferred.example/v1".into()),
            "OLLAMA_HOST" => Some("http://other.example/v1".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(preferred.endpoint, "https://preferred.example/v1");
    }

    #[test]
    fn validated_updates_persist_and_reload_after_reopening() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        settings
            .set(&store, "endpoint", " https://models.example/v1/ ")
            .unwrap();
        settings.set(&store, "model", " trimmed-model ").unwrap();
        settings.set(&store, "web", "ON").unwrap();
        assert_eq!(settings.endpoint, "https://models.example/v1");
        assert_eq!(settings.model, "trimmed-model");
        drop(store);
        let reopened = LocalStore::open(&directory.0).unwrap();
        assert_eq!(defaults(&reopened), settings);
        let saved: Value =
            serde_json::from_str(&reopened.setting(SETTINGS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved.as_object().unwrap().len(), 4);
        assert!(saved.get("apiKey").is_none());
    }

    #[test]
    fn invalid_updates_leave_memory_and_saved_settings_unchanged() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        settings.save(&store).unwrap();
        let before = settings.clone();
        let saved = store.setting(SETTINGS_KEY).unwrap();
        for (key, value) in [
            ("endpoint", "https://user:secret@models.example/v1"),
            ("endpoint", "https://models.example/v1?api_key=secret"),
            ("endpoint", "https://models.example/v1#secret"),
            ("endpoint", "file:///tmp/models"),
            ("endpoint", "http://models.example/\nsecret"),
            ("model", " "),
            ("model", "model\u{1b}[31m"),
            ("web", "yes"),
            ("memory", "sometimes"),
            ("apiKey", "secret"),
        ] {
            let error = settings.set(&store, key, value).unwrap_err();
            assert!(!error.contains("secret"));
            assert_eq!(settings, before);
            assert_eq!(store.setting(SETTINGS_KEY).unwrap(), saved);
        }
        assert!(settings.set(&store, "model", &"a".repeat(257)).is_err());
        assert_eq!(settings, before);
    }

    #[test]
    fn failed_persistence_does_not_publish_staged_settings() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        let before = settings.clone();
        let result = settings.set_with_persistence("model", "new-model", |updated| {
            assert_eq!(updated.model, "new-model");
            Err("The local database could not save settings.".into())
        });
        assert!(result.is_err());
        assert_eq!(settings, before);
        assert!(store.setting(SETTINGS_KEY).unwrap().is_none());
    }

    #[test]
    fn corrupt_saved_settings_have_safe_errors_without_fallback() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        for body in [
            "secret invalid JSON",
            "[]",
            "{}",
            r#"{"endpoint":7,"model":"test","webEnabled":false,"memoryEnabled":false}"#,
            r#"{"endpoint":"http://localhost:11434/v1","model":"test","webEnabled":"on","memoryEnabled":false}"#,
            r#"{"endpoint":"https://user:secret@example.com/v1","model":"test","webEnabled":false,"memoryEnabled":false}"#,
            r#"{"endpoint":"http://localhost:11434/v1","model":"test","webEnabled":false,"memoryEnabled":false,"apiKey":"secret"}"#,
        ] {
            store.save_setting(SETTINGS_KEY, body).unwrap();
            let error = load_with_environment(&store, |_| None).unwrap_err();
            assert!(error.starts_with("The saved CLI settings are invalid."));
            assert!(!error.contains("secret"));
            assert_eq!(store.setting(SETTINGS_KEY).unwrap().as_deref(), Some(body));
        }
    }

    #[test]
    fn invalid_desktop_preferences_are_reported_without_echoing_them() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        for body in [
            "secret invalid JSON",
            "[]",
            r#"{"endpoint":{"secret":"hidden"}}"#,
            r#"{"model":7}"#,
            r#"{"memoryEnabled":"secret"}"#,
        ] {
            store.save_setting("preferences", body).unwrap();
            let error = load_with_environment(&store, |_| None).unwrap_err();
            assert!(error.starts_with("The saved desktop"));
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn validation_is_atomic_and_descriptions_never_echo_credentials_or_controls() {
        let mut settings = Settings {
            endpoint: "models.example".into(),
            model: "\u{1b}[31msecret".into(),
            web_enabled: true,
            memory_enabled: false,
        };
        let before = settings.clone();
        assert!(settings.validate().is_err());
        assert_eq!(settings, before);
        settings.endpoint = "https://user:secret@models.example/v1".into();
        let description = settings.describe();
        assert!(!description.contains("secret"));
        assert!(!description.contains('\u{1b}'));
        assert!(description.contains("web: on\nmemory: off"));
    }
}
