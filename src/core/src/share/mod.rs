//! Secure, temporary browser sharing for a host-controlled model endpoint.
//!
//! A [`ShareHub`] owns at most one listener. Invite credentials are generated
//! from the operating system CSPRNG, returned only by [`ShareHub::start`], and
//! immediately reduced to a salted SHA-256 digest for in-memory verification.

mod gateway;
pub mod tailscale;

use std::{
    net::Ipv4Addr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use qrcode::{render::svg, QrCode};
use rand::{rngs::OsRng, RngCore};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

use self::{
    gateway::{salted_key_hash, start_gateway, GatewayConfig, GatewayControl},
    tailscale::detect_share_network,
};

/// Default TCP port dedicated to guest traffic.
pub const DEFAULT_SHARE_PORT: u16 = 11_435;
/// Environment variable overriding the URL advertised in invitations.
pub const SHARE_PUBLIC_URL_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_SHARE_PUBLIC_URL";
/// Environment variable overriding the dedicated listener port.
pub const SHARE_PORT_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_SHARE_PORT";
const MODEL_ENDPOINT_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_MODEL_ENDPOINT";
const OLLAMA_HOST_ENVIRONMENT_VARIABLE: &str = "OLLAMA_HOST";
const MODEL_API_KEY_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_MODEL_API_KEY";
const DEFAULT_MODEL_ENDPOINT: &str = "http://localhost:11434/v1";
const MIN_INVITE_MINUTES: u64 = 1;
const MAX_INVITE_MINUTES: u64 = 7 * 24 * 60;

/// UI request to create one temporary, model-pinned browser invite.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartShareRequest {
    /// Model identifier forced onto every guest completion request.
    pub model: String,
    /// Optional endpoint selected in the desktop UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Invite lifetime in minutes, from 1 minute through 7 days.
    pub expires_in_minutes: u64,
}

/// Current state of the guest gateway.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareStatus {
    /// Whether the listener is unrevoked and the invitation is unexpired.
    pub active: bool,
    /// Browser invitation URL. Populated only by `start_share` because it embeds
    /// the raw credential; subsequent status calls intentionally return `""`.
    pub share_url: String,
    /// SVG data URL encoding `share_url` byte-for-byte. Like `share_url`, this
    /// is returned once and is empty on later status calls.
    pub qr_data_url: String,
    /// Host-selected model guests are allowed to use.
    pub model: String,
    /// Invite expiry as milliseconds since the Unix epoch.
    pub expires_at: u64,
    /// Human-readable reachability scope for the bound interface.
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

/// Owns the process-wide guest listener and its revocable authorization state.
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
    /// Creates an idle hub. This does not start a model process or a listener.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Starts a temporary guest listener using existing endpoint environment
    /// configuration. Any previous listener is revoked before replacement.
    pub async fn start(&self, request: StartShareRequest) -> Result<ShareStatus, ShareError> {
        validate_request(&request)?;
        let network = detect_share_network();
        let port = configured_port()?;
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
        let public_base_url = environment_value(SHARE_PUBLIC_URL_ENVIRONMENT_VARIABLE)
            .map(|value| normalize_public_base_url(&value))
            .transpose()?;
        let config = ShareStartConfig {
            model: request.model,
            expires_in: Duration::from_secs(request.expires_in_minutes.saturating_mul(60)),
            upstream_endpoint,
            upstream_api_key: environment_value(MODEL_API_KEY_ENVIRONMENT_VARIABLE),
            bind_ip: network.bind_ip,
            port,
            public_base_url,
            network_label: if environment_value(SHARE_PUBLIC_URL_ENVIRONMENT_VARIABLE).is_some() {
                "Configured share URL".to_owned()
            } else {
                network.label.to_owned()
            },
        };
        self.start_configured(config).await
    }

    /// Returns metadata without returning the raw invite credential again.
    pub async fn status(&self) -> ShareStatus {
        let guard = self.inner.lock().await;
        let Some(running) = guard.as_ref() else {
            return ShareStatus::inactive();
        };
        ShareStatus {
            active: running.control.is_active() && !running.task.is_finished(),
            share_url: String::new(),
            qr_data_url: String::new(),
            model: running.model.clone(),
            expires_at: running.expires_at,
            network_label: running.network_label.clone(),
            request_count: running.control.request_count(),
        }
    }

    /// Revokes the current credential and closes the listener.
    pub async fn stop(&self) -> ShareStatus {
        let running = self.inner.lock().await.take();
        if let Some(running) = running {
            stop_running(running).await;
        }
        ShareStatus::inactive()
    }

    async fn start_configured(&self, config: ShareStartConfig) -> Result<ShareStatus, ShareError> {
        if config.model.trim().is_empty() {
            return Err(ShareError::MissingModel);
        }
        if config.expires_in.is_zero() {
            return Err(ShareError::InvalidExpiry {
                minimum_minutes: MIN_INVITE_MINUTES,
                maximum_minutes: MAX_INVITE_MINUTES,
            });
        }

        let mut guard = self.inner.lock().await;
        if let Some(running) = guard.take() {
            stop_running(running).await;
        }

        let expires_at = unix_epoch_millis()
            .saturating_add(config.expires_in.as_millis().try_into().unwrap_or(u64::MAX));
        let mut key_bytes = [0_u8; 32];
        let mut salt = [0_u8; 32];
        OsRng.fill_bytes(&mut key_bytes);
        OsRng.fill_bytes(&mut salt);
        let raw_key = format!("bw1_{}", URL_SAFE_NO_PAD.encode(key_bytes));
        key_bytes.fill(0);
        let key_hash = salted_key_hash(&salt, &raw_key);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10 * 60))
            .user_agent(concat!("blackwall-share/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(ShareError::Client)?;
        let gateway = start_gateway(GatewayConfig {
            client,
            upstream_endpoint: config.upstream_endpoint,
            upstream_api_key: config.upstream_api_key,
            pinned_model: config.model.clone(),
            bind_ip: config.bind_ip,
            port: config.port,
            salt,
            key_hash,
            expires_at_ms: expires_at,
        })
        .await
        .map_err(ShareError::Listener)?;

        let advertised_base = match config.public_base_url {
            Some(base) => base,
            None => format!(
                "http://{}:{}",
                gateway.local_addr.ip(),
                gateway.local_addr.port()
            ),
        };
        let share_url = format!("{advertised_base}/guest#key={raw_key}");
        let qr_data_url = qr_data_url(&share_url)?;
        let status = ShareStatus {
            active: true,
            share_url,
            qr_data_url,
            model: config.model.clone(),
            expires_at,
            network_label: config.network_label.clone(),
            request_count: 0,
        };
        *guard = Some(RunningShare {
            model: config.model,
            expires_at,
            network_label: config.network_label,
            control: gateway.control,
            shutdown: gateway.shutdown,
            task: gateway.task,
        });
        Ok(status)
    }
}

struct ShareStartConfig {
    model: String,
    expires_in: Duration,
    upstream_endpoint: String,
    upstream_api_key: Option<String>,
    bind_ip: Ipv4Addr,
    port: u16,
    public_base_url: Option<String>,
    network_label: String,
}

struct RunningShare {
    model: String,
    expires_at: u64,
    network_label: String,
    control: GatewayControl,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn stop_running(running: RunningShare) {
    running.control.revoke();
    let _send_result = running.shutdown.send(());
    // The gateway supervisor owns a blocking socket bridge. Tokio cannot
    // safely cancel an already-running blocking task, so always let the
    // supervisor close both socket sides and join every worker before return.
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

fn configured_port() -> Result<u16, ShareError> {
    match environment_value(SHARE_PORT_ENVIRONMENT_VARIABLE) {
        Some(value) => {
            value
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or(ShareError::InvalidPort {
                    value,
                    variable: SHARE_PORT_ENVIRONMENT_VARIABLE,
                })
        }
        None => Ok(DEFAULT_SHARE_PORT),
    }
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
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ShareError::InvalidEndpoint {
            endpoint: endpoint.to_owned(),
            reason: "credentials must not be embedded in the URL".to_owned(),
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

fn normalize_public_base_url(value: &str) -> Result<String, ShareError> {
    let mut url = Url::parse(value.trim()).map_err(|error| ShareError::InvalidPublicUrl {
        value: value.to_owned(),
        reason: error.to_string(),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ShareError::InvalidPublicUrl {
            value: value.to_owned(),
            reason: "only http and https URLs are supported".to_owned(),
        });
    }
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(ShareError::InvalidPublicUrl {
            value: value.to_owned(),
            reason: "the URL must be an origin with no credentials, path, query, or fragment"
                .to_owned(),
        });
    }
    url.set_path("");
    Ok(url.to_string().trim_end_matches('/').to_owned())
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
    /// Listener port environment configuration is malformed.
    #[error("{variable} must be a non-zero TCP port, not {value:?}")]
    InvalidPort {
        /// Invalid environment value.
        value: String,
        /// Stable environment variable name.
        variable: &'static str,
    },
    /// The host model endpoint cannot be used safely.
    #[error("invalid model endpoint {endpoint:?}: {reason}")]
    InvalidEndpoint {
        /// Rejected endpoint.
        endpoint: String,
        /// Validation detail.
        reason: String,
    },
    /// The advertised share URL override is malformed.
    #[error("invalid public share URL {value:?}: {reason}")]
    InvalidPublicUrl {
        /// Rejected override.
        value: String,
        /// Validation detail.
        reason: String,
    },
    /// The bounded upstream HTTP client could not be created.
    #[error("could not initialize the guest model client: {0}")]
    Client(#[source] reqwest::Error),
    /// The exact Tailscale or loopback listener address could not be opened.
    #[error("could not start the guest listener: {0}")]
    Listener(#[source] std::io::Error),
    /// Invite payload exceeded QR encoding limits.
    #[error("could not encode the invite QR: {0}")]
    Qr(#[source] qrcode::types::QrError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn serde_contract_uses_ui_field_names_and_epoch_milliseconds() {
        let request: StartShareRequest = serde_json::from_value(serde_json::json!({
            "model": "qwen3",
            "endpoint": "http://model-host:11434",
            "expiresInMinutes": 60
        }))
        .unwrap();
        assert_eq!(request.expires_in_minutes, 60);
        assert_eq!(request.endpoint.as_deref(), Some("http://model-host:11434"));

        let value = serde_json::to_value(ShareStatus {
            active: true,
            share_url: "http://100.64.1.2:11435/guest#key=bw1_test".to_owned(),
            qr_data_url: "data:image/svg+xml;base64,PHN2Zz4=".to_owned(),
            model: "qwen3".to_owned(),
            expires_at: 1_787_680_000_000,
            network_label: "Tailscale tailnet".to_owned(),
            request_count: 2,
        })
        .unwrap();
        assert_eq!(value["expiresAt"], 1_787_680_000_000_u64);
        assert!(value.get("shareUrl").is_some());
        assert!(value.get("qrDataUrl").is_some());
        assert!(value.get("requestCount").is_some());
    }

    #[test]
    fn endpoint_and_public_url_normalization_are_bounded() {
        assert_eq!(
            normalize_upstream_endpoint("192.0.2.42:11434").unwrap(),
            "http://192.0.2.42:11434/v1"
        );
        assert_eq!(
            normalize_upstream_endpoint("https://model.example/v1/").unwrap(),
            "https://model.example/v1"
        );
        assert!(normalize_upstream_endpoint("file:///tmp/model").is_err());
        assert!(normalize_upstream_endpoint("http://user:pass@host/v1").is_err());
        assert_eq!(
            normalize_public_base_url("https://host.example/").unwrap(),
            "https://host.example"
        );
        assert!(normalize_public_base_url("https://host.example/share/").is_err());
        assert!(normalize_public_base_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn qr_is_an_svg_data_url_generated_from_the_exact_link() {
        let link = "http://100.64.1.2:11435/guest#key=bw1_byte_exact";
        let data_url = qr_data_url(link).unwrap();
        let encoded = data_url.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let svg = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let svg = String::from_utf8(svg).unwrap();
        assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"));

        let expected_code = QrCode::new(link.as_bytes()).unwrap();
        let repeated_code = QrCode::new(link.as_bytes()).unwrap();
        assert_eq!(
            expected_code.render::<svg::Color>().build(),
            repeated_code.render::<svg::Color>().build()
        );
    }
}
