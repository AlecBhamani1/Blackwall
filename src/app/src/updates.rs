//! Channel endpoints stay native; the webview cannot supply an arbitrary update source.
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tauri::{Manager, ResourceId, Webview};
use tauri_plugin_updater::UpdaterExt;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    Stable,
    Beta,
}

impl UpdateChannel {
    fn should_offer(self, current_prerelease: &str, remote_prerelease: &str, newer: bool) -> bool {
        match self {
            // Selecting Stable explicitly offers a return from beta, even if the
            // latest production version is older. Installation still requires consent.
            Self::Stable => {
                remote_prerelease.is_empty() && (newer || current_prerelease.starts_with("beta."))
            }
            Self::Beta => remote_prerelease.starts_with("beta.") && newer,
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::Stable => {
                "https://github.com/AlecBhamani1/Blackwall/releases/download/main/latest.json"
            }
            Self::Beta => {
                "https://github.com/AlecBhamani1/Blackwall/releases/download/beta/latest.json"
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMetadata {
    rid: ResourceId,
    current_version: String,
    version: String,
    date: Option<String>,
    body: Option<String>,
    raw_json: serde_json::Value,
}

#[tauri::command]
pub async fn check_app_update(
    webview: Webview,
    channel: UpdateChannel,
) -> Result<Option<UpdateMetadata>, String> {
    let endpoint = reqwest::Url::parse(channel.endpoint()).map_err(|e| e.to_string())?;
    let updater = webview
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?
        .version_comparator(move |current, remote| {
            channel.should_offer(
                current.pre.as_str(),
                remote.version.pre.as_str(),
                remote.version > current,
            )
        })
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let metadata = UpdateMetadata {
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        date: update
            .raw_json
            .get("pub_date")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        body: update.body.clone(),
        raw_json: update.raw_json.clone(),
        // Reuse the plugin's download/install commands and signature verification.
        rid: webview.resources_table().add(update),
    };
    Ok(Some(metadata))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_channels_are_accepted() {
        assert!(serde_json::from_str::<UpdateChannel>("\"stable\"").is_ok());
        assert!(serde_json::from_str::<UpdateChannel>("\"beta\"").is_ok());
        assert!(serde_json::from_str::<UpdateChannel>("\"https://example.com\"").is_err());
        assert_ne!(
            UpdateChannel::Stable.endpoint(),
            UpdateChannel::Beta.endpoint()
        );
    }

    #[test]
    fn beta_stays_on_newer_previews() {
        assert!(UpdateChannel::Beta.should_offer("", "beta.42", true));
        assert!(UpdateChannel::Beta.should_offer("beta.41", "beta.42", true));
        assert!(!UpdateChannel::Beta.should_offer("beta.42", "beta.41", false));
        assert!(!UpdateChannel::Beta.should_offer("beta.42", "beta.42", false));
        assert!(!UpdateChannel::Beta.should_offer("beta.42", "", true));
        assert!(!UpdateChannel::Beta.should_offer("", "rc.1", true));
    }

    #[test]
    fn only_stable_selection_allows_returning_from_beta() {
        assert!(UpdateChannel::Stable.should_offer("beta.42", "", false));
        assert!(UpdateChannel::Stable.should_offer("beta.42", "", true));
        assert!(UpdateChannel::Stable.should_offer("", "", true));
        assert!(!UpdateChannel::Stable.should_offer("", "", false));
        assert!(!UpdateChannel::Stable.should_offer("", "beta.42", true));
        assert!(!UpdateChannel::Stable.should_offer("rc.1", "", false));
    }
}
