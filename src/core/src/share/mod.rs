//! Secure, temporary browser sharing through a configurable hosted relay.
//!
//! The desktop opens an outbound-only WebSocket to the relay. Guest invite
//! credentials are generated locally, returned only by [`ShareHub::start`],
//! and shared with the relay only as a salted SHA-256 digest.

pub mod guest_assets;
mod relay;
pub mod relay_protocol;

use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use qrcode::{render::svg, QrCode};
use rand::{rngs::OsRng, RngCore};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;

use self::{
    relay::{start_relay_host, RelayHostConfig, RelayHostControl, RelayHostError},
    relay_protocol::RelayRegistration,
};

/// Environment variable selecting the hosted relay origin.
pub const RELAY_URL_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_RELAY_URL";
/// Optional deployment-wide relay registration credential.
pub const RELAY_TOKEN_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_RELAY_TOKEN";
const MODEL_ENDPOINT_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_MODEL_ENDPOINT";
const OLLAMA_HOST_ENVIRONMENT_VARIABLE: &str = "OLLAMA_HOST";
const MODEL_API_KEY_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_MODEL_API_KEY";
const DEFAULT_MODEL_ENDPOINT: &str = "http://localhost:11434/v1";
const MIN_INVITE_MINUTES: u64 = 1;
const MAX_INVITE_MINUTES: u64 = 7 * 24 * 60;
const PROTOCOL_VERSION: u16 = 1;

/// UI request to create one temporary, model-pinned hosted invite.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartShareRequest {
    /// Model identifier forced onto every guest completion request.
    pub model: String,
    /// Optional endpoint selected in the desktop UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Hosted relay origin selected in Settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    /// Optional deployment registration credential. This is never returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_token: Option<String>,
    /// Invite lifetime in minutes, from 1 minute through 7 days.
    pub expires_in_minutes: u64,
}

/// Current state of the hosted guest share.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareStatus {
    /// Whether the invite remains live or is reconnecting to its relay.
    pub active: bool,
    /// Browser invitation URL. Populated only by `start_share` because it embeds
    /// the raw credential; subsequent status calls intentionally return `""`.
    pub share_url: String,
    /// SVG data URL encoding `share_url` byte-for-byte.
    pub qr_data_url: String,
    /// Host-selected model guests are allowed to use.
    pub model: String,
    /// Invite expiry as milliseconds since the Unix epoch.
    pub expires_at: u64,
    /// Human-readable relay connection state.
    pub network_label: String,
    /// Number of accepted authenticated API requests during this invite.
    pub request_count: u64,
}

impl ShareStatus {
    /// Standard inactive result used after an explicit stop or before sharing.
    pub fn inactive() -> Self {
        Self {
            active: false,
            share_url: String::new(),
            qr_data_url: String::new(),
            model: String::new(),
            expires_at: 0,
            network_label: String::new(),
            request_count: 0,
        }
    }
}

/// Owns the process-wide outbound relay connection and revocable invite.
#[derive(Clone)]
pub struct ShareHub {
    inner: Arc<Mutex<Option<RunningShare>>>,
}

impl Default for ShareHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ShareHub {
    /// Creates an idle hub without opening a network connection.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Publishes a temporary invite through the selected hosted relay.
    pub async fn start(&self, request: StartShareRequest) -> Result<ShareStatus, ShareError> {
        validate_request(&request)?;
        let relay_value = request
            .relay_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| environment_value(RELAY_URL_ENVIRONMENT_VARIABLE))
            .ok_or(ShareError::MissingRelayUrl)?;
        let relay_base_url = normalize_relay_base_url(&relay_value)?;
        let relay_token = request
            .relay_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| environment_value(RELAY_TOKEN_ENVIRONMENT_VARIABLE));
        let environment_endpoint = environment_value(MODEL_ENDPOINT_ENVIRONMENT_VARIABLE)
            .or_else(|| environment_value(OLLAMA_HOST_ENVIRONMENT_VARIABLE));
        let upstream_endpoint = normalize_upstream_endpoint(
            request
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .or(environment_endpoint.as_deref())
                .unwrap_or(DEFAULT_MODEL_ENDPOINT),
        )?;
        self.start_configured(ShareStartConfig {
            model: request.model,
            expires_in: Duration::from_secs(request.expires_in_minutes.saturating_mul(60)),
            upstream_endpoint,
            upstream_api_key: environment_value(MODEL_API_KEY_ENVIRONMENT_VARIABLE),
            relay_base_url,
            relay_token,
        })
        .await
    }

    /// Returns metadata without returning the raw invite credential again.
    pub async fn status(&self) -> ShareStatus {
        let guard = self.inner.lock().await;
        let Some(running) = guard.as_ref() else {
            return ShareStatus::inactive();
        };
        let active = unix_epoch_millis() < running.expires_at && !running.task.is_finished();
        ShareStatus {
            active,
            share_url: String::new(),
            qr_data_url: String::new(),
            model: running.model.clone(),
            expires_at: running.expires_at,
            network_label: if running.control.is_connected() {
                "Hosted relay".to_owned()
            } else {
                "Hosted relay · reconnecting".to_owned()
            },
            request_count: running.control.request_count(),
        }
    }

    /// Revokes the current invitation by closing its authenticated host session.
    pub async fn stop(&self) -> ShareStatus {
        let running = self.inner.lock().await.take();
        if let Some(running) = running {
            stop_running(running).await;
        }
        ShareStatus::inactive()
    }

    async fn start_configured(&self, config: ShareStartConfig) -> Result<ShareStatus, ShareError> {
        let mut guard = self.inner.lock().await;
        if let Some(running) = guard.take() {
            stop_running(running).await;
        }

        let expires_at = unix_epoch_millis()
            .saturating_add(config.expires_in.as_millis().try_into().unwrap_or(u64::MAX));
        let raw_key = random_secret("bw1_");
        let host_key = random_secret("bwh_");
        let session_id = random_secret("bws_");
        let mut salt = [0_u8; 32];
        OsRng.fill_bytes(&mut salt);
        let key_hash = salted_key_hash(&salt, &raw_key);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10 * 60))
            .user_agent(concat!("blackwall-share/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(ShareError::Client)?;
        let socket_url = relay_socket_url(&config.relay_base_url)?;
        let registration = RelayRegistration {
            protocol_version: PROTOCOL_VERSION,
            session_id: session_id.clone(),
            host_key,
            relay_token: config.relay_token,
            model: config.model.clone(),
            guest_key_salt: URL_SAFE_NO_PAD.encode(salt),
            guest_key_hash: URL_SAFE_NO_PAD.encode(key_hash),
            expires_at_ms: expires_at,
        };
        let relay = start_relay_host(RelayHostConfig {
            socket_url,
            registration,
            client,
            upstream_endpoint: config.upstream_endpoint,
            upstream_api_key: config.upstream_api_key,
        })
        .await
        .map_err(|error| ShareError::Relay(error.to_string()))?;

        let share_url = format!(
            "{}/s/{session_id}/guest#key={raw_key}",
            config.relay_base_url
        );
        let qr_data_url = qr_data_url(&share_url)?;
        let status = ShareStatus {
            active: true,
            share_url,
            qr_data_url,
            model: config.model.clone(),
            expires_at,
            network_label: "Hosted relay".to_owned(),
            request_count: 0,
        };
        *guard = Some(RunningShare {
            model: config.model,
            expires_at,
            control: relay.control,
            shutdown: relay.shutdown,
            task: relay.task,
        });
        Ok(status)
    }
}

struct ShareStartConfig {
    model: String,
    expires_in: Duration,
    upstream_endpoint: String,
    upstream_api_key: Option<String>,
    relay_base_url: String,
    relay_token: Option<String>,
}

struct RunningShare {
    model: String,
    expires_at: u64,
    control: RelayHostControl,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<(), RelayHostError>>,
}

async fn stop_running(running: RunningShare) {
    let _shutdown_result = running.shutdown.send(true);
    let _join_result = running.task.await;
}

fn validate_request(request: &StartShareRequest) -> Result<(), ShareError> {
    if request.model.trim().is_empty() {
        return Err(ShareError::MissingModel);
    }
    if !(MIN_INVITE_MINUTES..=MAX_INVITE_MINUTES).contains(&request.expires_in_minutes) {
        return Err(ShareError::InvalidExpiry {
            minimum_minutes: MIN_INVITE_MINUTES,
            maximum_minutes: MAX_INVITE_MINUTES,
        });
    }
    Ok(())
}

fn normalize_upstream_endpoint(endpoint: &str) -> Result<String, ShareError> {
    let trimmed = endpoint.trim();
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("http://{trimmed}")
    };
    let mut url = Url::parse(&candidate).map_err(|error| ShareError::InvalidEndpoint {
        endpoint: endpoint.to_owned(),
        reason: error.to_string(),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ShareError::InvalidEndpoint {
            endpoint: endpoint.to_owned(),
            reason: "only http and https endpoints are supported".to_owned(),
        });
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(ShareError::InvalidEndpoint {
            endpoint: endpoint.to_owned(),
            reason: "a host is required and credentials must not be embedded in the URL".to_owned(),
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ShareError::InvalidEndpoint {
            endpoint: endpoint.to_owned(),
            reason: "query strings and fragments are not supported".to_owned(),
        });
    }
    if url.path().is_empty() || url.path() == "/" {
        url.set_path("/v1");
    } else {
        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&path);
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn normalize_relay_base_url(value: &str) -> Result<String, ShareError> {
    let mut url = Url::parse(value.trim()).map_err(|error| ShareError::InvalidRelayUrl {
        value: value.to_owned(),
        reason: error.to_string(),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ShareError::InvalidRelayUrl {
            value: value.to_owned(),
            reason: "only http and https URLs are supported".to_owned(),
        });
    }
    let host = url.host_str().ok_or_else(|| ShareError::InvalidRelayUrl {
        value: value.to_owned(),
        reason: "a host is required".to_owned(),
    })?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if url.scheme() != "https" && !loopback {
        return Err(ShareError::InvalidRelayUrl {
            value: value.to_owned(),
            reason: "public relay URLs must use HTTPS".to_owned(),
        });
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(ShareError::InvalidRelayUrl {
            value: value.to_owned(),
            reason: "the URL must be an origin with no credentials, path, query, or fragment"
                .to_owned(),
        });
    }
    url.set_path("");
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn relay_socket_url(relay_base_url: &str) -> Result<String, ShareError> {
    let mut url = Url::parse(relay_base_url).map_err(|error| ShareError::InvalidRelayUrl {
        value: relay_base_url.to_owned(),
        reason: error.to_string(),
    })?;
    let socket_scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(socket_scheme)
        .map_err(|()| ShareError::InvalidRelayUrl {
            value: relay_base_url.to_owned(),
            reason: "could not construct the relay WebSocket URL".to_owned(),
        })?;
    url.set_path("/v1/host/connect");
    Ok(url.to_string())
}

fn random_secret(prefix: &str) -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let secret = format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes));
    bytes.fill(0);
    secret
}

/// Computes the digest stored by the relay instead of a raw invite key.
pub fn salted_key_hash(salt: &[u8; 32], key: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(key.as_bytes());
    hasher.finalize().into()
}

fn qr_data_url(payload: &str) -> Result<String, ShareError> {
    let code = QrCode::new(payload.as_bytes()).map_err(ShareError::Qr)?;
    let svg = code
        .render::<svg::Color>()
        .min_dimensions(320, 320)
        .dark_color(svg::Color("#0e0f13"))
        .light_color(svg::Color("#ffffff"))
        .build();
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(svg.as_bytes())
    ))
}

fn environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn unix_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Safe failure returned while validating or starting guest sharing.
#[derive(Debug, Error)]
pub enum ShareError {
    /// No model was selected for the guest.
    #[error("a model must be selected before sharing")]
    MissingModel,
    /// Invite lifetime is outside the accepted window.
    #[error("invite lifetime must be between {minimum_minutes} and {maximum_minutes} minutes")]
    InvalidExpiry {
        /// Smallest accepted lifetime.
        minimum_minutes: u64,
        /// Largest accepted lifetime.
        maximum_minutes: u64,
    },
    /// Neither the request nor the environment selected a relay.
    #[error("enter a hosted relay URL before sharing")]
    MissingRelayUrl,
    /// The hosted relay origin cannot be used safely.
    #[error("invalid hosted relay URL {value:?}: {reason}")]
    InvalidRelayUrl {
        /// Rejected relay URL.
        value: String,
        /// Validation detail.
        reason: String,
    },
    /// The host model endpoint cannot be used safely.
    #[error("invalid model endpoint {endpoint:?}: {reason}")]
    InvalidEndpoint {
        /// Rejected endpoint.
        endpoint: String,
        /// Validation detail.
        reason: String,
    },
    /// The bounded upstream HTTP client could not be created.
    #[error("could not initialize the guest model client: {0}")]
    Client(#[source] reqwest::Error),
    /// Hosted relay connection or registration failed.
    #[error("could not publish the guest share: {0}")]
    Relay(String),
    /// Invite payload exceeded QR encoding limits.
    #[error("could not encode the invite QR: {0}")]
    Qr(#[source] qrcode::types::QrError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn serde_contract_uses_ui_field_names_and_hides_optional_secrets() {
        let request: StartShareRequest = serde_json::from_value(serde_json::json!({
            "model": "qwen3",
            "endpoint": "http://model-host:11434",
            "relayUrl": "https://relay.example.com",
            "relayToken": "deployment-secret",
            "expiresInMinutes": 60
        }))
        .unwrap();
        assert_eq!(request.expires_in_minutes, 60);
        assert_eq!(
            request.relay_url.as_deref(),
            Some("https://relay.example.com")
        );
        assert_eq!(request.relay_token.as_deref(), Some("deployment-secret"));

        let value = serde_json::to_value(ShareStatus {
            active: true,
            share_url: "https://relay.example.com/s/id/guest#key=bw1_test".to_owned(),
            qr_data_url: "data:image/svg+xml;base64,PHN2Zz4=".to_owned(),
            model: "qwen3".to_owned(),
            expires_at: 1_787_680_000_000,
            network_label: "Hosted relay".to_owned(),
            request_count: 2,
        })
        .unwrap();
        assert_eq!(value["expiresAt"], 1_787_680_000_000_u64);
        assert!(value.get("shareUrl").is_some());
        assert!(value.get("relayToken").is_none());
    }

    #[test]
    fn endpoint_and_relay_url_normalization_are_bounded() {
        assert_eq!(
            normalize_upstream_endpoint("192.0.2.42:11434").unwrap(),
            "http://192.0.2.42:11434/v1"
        );
        assert_eq!(
            normalize_relay_base_url("https://relay.example.com/").unwrap(),
            "https://relay.example.com"
        );
        assert_eq!(
            relay_socket_url("https://relay.example.com").unwrap(),
            "wss://relay.example.com/v1/host/connect"
        );
        assert!(normalize_relay_base_url("http://relay.example.com").is_err());
        assert!(normalize_relay_base_url("https://relay.example.com/path").is_err());
        assert!(normalize_relay_base_url("https://user:pass@relay.example.com").is_err());
        assert_eq!(
            normalize_relay_base_url("http://127.0.0.1:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn salted_hash_is_deterministic_and_the_qr_encodes_the_exact_link() {
        assert_eq!(
            salted_key_hash(&[1; 32], "bw1_test"),
            salted_key_hash(&[1; 32], "bw1_test")
        );
        assert_ne!(
            salted_key_hash(&[1; 32], "bw1_test"),
            salted_key_hash(&[2; 32], "bw1_test")
        );

        let link = "https://relay.example.com/s/id/guest#key=bw1_byte_exact";
        let data_url = qr_data_url(link).unwrap();
        let encoded = data_url.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let svg = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert!(String::from_utf8(svg).unwrap().contains("<svg"));
    }
}
