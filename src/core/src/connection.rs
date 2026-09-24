//! Bounded, credential-free discovery and explicit local model downloads.
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_BODY: usize = 1024 * 1024;
const MAX_FRAME: usize = 16 * 1024;
pub const LOCAL_OLLAMA: &str = "http://127.0.0.1:11434";

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error("Enter a valid computer address using HTTP or HTTPS.")]
    InvalidAddress,
    #[error("Keep passwords and access keys out of the computer address.")]
    EmbeddedCredentials,
    #[error("The model service could not be reached. Open it on this computer and try again.")]
    Unavailable,
    #[error("The model service sent an unexpected response. Update it and try again.")]
    InvalidResponse,
    #[error("Choose one of the models offered in setup.")]
    UnsupportedModel,
    #[error("The download did not finish. Check your internet connection and try again.")]
    DownloadFailed,
}

/// Normalize a user-supplied base URL without accepting credentials or query secrets.
pub fn normalize_endpoint(value: &str) -> Result<String, ConnectionError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 {
        return Err(ConnectionError::InvalidAddress);
    }
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    let mut url = Url::parse(&candidate).map_err(|_| ConnectionError::InvalidAddress)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ConnectionError::InvalidAddress);
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConnectionError::EmbeddedCredentials);
    }
    if url.path() == "/" {
        url.set_path("/v1");
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

/// Whether two validated endpoints may share an origin-scoped credential.
pub fn same_origin(left: &str, right: &str) -> bool {
    match (Url::parse(left), Url::parse(right)) {
        (Ok(left), Ok(right)) => left.origin() == right.origin(),
        _ => false,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalService {
    pub name: String,
    pub endpoint: String,
    pub available: bool,
    pub models: Vec<String>,
    pub supports_download: bool,
}

fn client() -> Result<Client, ConnectionError> {
    Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| ConnectionError::Unavailable)
}

async fn bounded_json(response: reqwest::Response) -> Result<serde_json::Value, ConnectionError> {
    if !response.status().is_success() {
        return Err(ConnectionError::Unavailable);
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ConnectionError::Unavailable)?;
        if body.len() + chunk.len() > MAX_BODY {
            return Err(ConnectionError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| ConnectionError::InvalidResponse)
}

async fn probe(name: &str, origin: &str, ollama: bool) -> LocalService {
    let mut result = LocalService {
        name: name.into(),
        endpoint: format!("{origin}/v1"),
        available: false,
        models: vec![],
        supports_download: ollama,
    };
    let fetch = async {
        let response = client()?
            .get(format!(
                "{origin}/{}",
                if ollama { "api/tags" } else { "v1/models" }
            ))
            .send()
            .await
            .map_err(|_| ConnectionError::Unavailable)?;
        bounded_json(response).await
    };
    if let Ok(Ok(value)) = tokio::time::timeout(Duration::from_secs(3), fetch).await {
        if let Some(items) = value
            .get(if ollama { "models" } else { "data" })
            .and_then(|v| v.as_array())
        {
            result.available = true;
            result.models = items
                .iter()
                .filter_map(|item| {
                    item.get(if ollama { "name" } else { "id" })
                        .and_then(|v| v.as_str())
                })
                .filter(|name| !name.trim().is_empty() && name.len() <= 256)
                .take(200)
                .map(str::to_owned)
                .collect();
        }
    }
    result
}

/// Only probes known loopback services. Never scans a network or uses remote credentials.
pub async fn discover_local_services() -> Vec<LocalService> {
    let (ollama, studio) = tokio::join!(
        probe("Ollama", LOCAL_OLLAMA, true),
        probe("LM Studio", "http://127.0.0.1:1234", false)
    );
    vec![ollama, studio]
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub status: String,
    #[serde(default)]
    pub completed: u64,
    #[serde(default)]
    pub total: u64,
}

/// Incremental NDJSON decoding, including split UTF-8, bounds, and unterminated final frames.
#[derive(Default)]
pub struct ProgressDecoder {
    pending: Vec<u8>,
}
impl ProgressDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<DownloadProgress>, ConnectionError> {
        let mut frames = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                if let Some(frame) = self.finish()? {
                    frames.push(frame);
                }
            } else {
                if self.pending.len() >= MAX_FRAME {
                    return Err(ConnectionError::InvalidResponse);
                }
                self.pending.push(*byte);
            }
        }
        Ok(frames)
    }
    pub fn finish(&mut self) -> Result<Option<DownloadProgress>, ConnectionError> {
        let bytes = std::mem::take(&mut self.pending);
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| ConnectionError::InvalidResponse)?;
        if value.get("error").is_some() {
            return Err(ConnectionError::DownloadFailed);
        }
        let mut progress: DownloadProgress =
            serde_json::from_value(value).map_err(|_| ConnectionError::InvalidResponse)?;
        if progress.status.len() > 256 {
            progress.status = progress.status.chars().take(128).collect();
        }
        Ok(Some(progress))
    }
}

/// Pull a reviewed small text model into the local Ollama service. Dropping the future closes
/// this request; Ollama may continue a download already shared with another client.
pub async fn download_model(
    model: &str,
    mut progress: impl FnMut(DownloadProgress),
) -> Result<(), ConnectionError> {
    if !matches!(model, "llama3.2:1b" | "llama3.2:3b") {
        return Err(ConnectionError::UnsupportedModel);
    }
    let response = client()?
        .post(format!("{LOCAL_OLLAMA}/api/pull"))
        .json(&serde_json::json!({"model":model,"stream":true}))
        .send()
        .await
        .map_err(|_| ConnectionError::Unavailable)?;
    if !response.status().is_success() {
        return Err(ConnectionError::DownloadFailed);
    }
    let mut stream = response.bytes_stream();
    let mut decoder = ProgressDecoder::default();
    let mut success = false;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(120), stream.next())
        .await
        .map_err(|_| ConnectionError::DownloadFailed)?
    {
        for frame in decoder.push(&chunk.map_err(|_| ConnectionError::DownloadFailed)?)? {
            success |= frame.status == "success";
            progress(frame);
        }
    }
    if let Some(frame) = decoder.finish()? {
        success |= frame.status == "success";
        progress(frame);
    }
    if success {
        Ok(())
    } else {
        Err(ConnectionError::DownloadFailed)
    }
}

/// Validate existing Blackwall invitation URLs before opening them externally.
pub fn validate_invitation(value: &str) -> Result<Url, ConnectionError> {
    let url = Url::parse(value.trim()).map_err(|_| ConnectionError::InvalidAddress)?;
    let path: Vec<_> = url.path().split('/').collect();
    let valid_key = url
        .fragment()
        .and_then(|f| f.strip_prefix("key=bw1_"))
        .is_some_and(valid_secret);
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || path.len() != 4
        || path[1] != "s"
        || !path[2].strip_prefix("bws_").is_some_and(valid_secret)
        || path[3] != "guest"
        || !valid_key
    {
        return Err(ConnectionError::InvalidAddress);
    }
    Ok(url)
}
fn valid_secret(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_validation_and_credential_scoping() {
        assert_eq!(
            normalize_endpoint("localhost:11434").unwrap(),
            "http://localhost:11434/v1"
        );
        for bad in [
            "",
            "file:///etc/passwd",
            "https://a:password@host",
            "https://host?key=secret",
            "https://host#secret",
        ] {
            assert!(normalize_endpoint(bad).is_err());
        }
        assert!(same_origin("https://host/v1", "https://host/api"));
        assert!(!same_origin("https://host", "http://host"));
        assert!(!same_origin("https://host", "https://another"));
    }
    #[test]
    fn download_frames_are_bounded_and_errors_are_not_progress() {
        let mut decoder = ProgressDecoder::default();
        assert!(decoder.push(b"{\"status\":\"pull").unwrap().is_empty());
        let frames = decoder
            .push(b"ing\",\"completed\":5,\"total\":10}\n{\"status\":\"success\"}")
            .unwrap();
        assert_eq!(frames[0].completed, 5);
        assert_eq!(decoder.finish().unwrap().unwrap().status, "success");
        assert!(decoder
            .push(b"{\"error\":\"private server details\"}\n")
            .is_err());
        assert!(ProgressDecoder::default()
            .push(&vec![b'x'; MAX_FRAME + 1])
            .is_err());
    }
    #[test]
    fn invitations_reject_unsafe_schemes_and_malformed_secrets() {
        let good = format!(
            "https://relay.example/s/bws_{}/guest#key=bw1_{}",
            "a".repeat(43),
            "b".repeat(43)
        );
        assert!(validate_invitation(&good).is_ok());
        assert!(validate_invitation(&good.replace("https:", "http:")).is_err());
        assert!(validate_invitation("javascript:alert(1)").is_err());
        assert!(validate_invitation("https://relay.example/s/id/guest#key=bw1_short").is_err());
    }
}
