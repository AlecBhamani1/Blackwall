//! Bounded, credential-free discovery and explicit local model downloads.
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::ContextBudget;

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
            result.models = model_names(items, ollama);
        }
    }
    result
}

/// The kind of model server behind an endpoint, as found by [`probe_server`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ServerKind {
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "llama.cpp")]
    LlamaCpp,
    #[serde(rename = "vllm")]
    Vllm,
    #[serde(rename = "lm-studio")]
    LmStudio,
    #[serde(rename = "unknown")]
    Unknown,
}

impl ServerKind {
    /// The order servers are recognised in when more than one route answers.
    const PROBED: [ServerKind; 4] = [Self::Ollama, Self::LlamaCpp, Self::Vllm, Self::LmStudio];

    /// A stable lowercase name for settings, traces, and CLI output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::LlamaCpp => "llama.cpp",
            Self::Vllm => "vllm",
            Self::LmStudio => "lm-studio",
            Self::Unknown => "unknown",
        }
    }
}

/// Server kinds already recognised this session, by endpoint. A recognised server is asked
/// only its own route next time; unknown servers are asked everything again.
fn known_kinds() -> &'static std::sync::Mutex<std::collections::HashMap<String, ServerKind>> {
    static KINDS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, ServerKind>>,
    > = std::sync::OnceLock::new();
    KINDS.get_or_init(Default::default)
}

async fn fetch_json(url: &str) -> Result<serde_json::Value, ConnectionError> {
    let response = client()?
        .get(url)
        .send()
        .await
        .map_err(|_| ConnectionError::Unavailable)?;
    bounded_json(response).await
}

/// Ask one kind of server for `model`'s context window. `None` means the endpoint is not that
/// kind; `Some(None)` means it is, but reported no window for `model`.
async fn probe_kind(
    kind: ServerKind,
    endpoint: &str,
    origin: Option<&str>,
    model: &str,
) -> Option<Option<u64>> {
    let probe = async {
        match kind {
            ServerKind::Ollama => {
                let origin = origin?;
                let version = fetch_json(&format!("{origin}/api/version")).await.ok()?;
                version.get("version")?.as_str()?;
                Some(
                    fetch_json(&format!("{origin}/api/ps"))
                        .await
                        .ok()
                        .and_then(|ps| ollama_window(&ps, model)),
                )
            }
            ServerKind::LlamaCpp => {
                let props = fetch_json(&format!("{}/props", origin?)).await.ok()?;
                Some(Some(
                    props
                        .get("default_generation_settings")?
                        .get("n_ctx")?
                        .as_u64()?,
                ))
            }
            ServerKind::Vllm => {
                let models = fetch_json(&format!("{endpoint}/models")).await.ok()?;
                let items = models.get("data")?.as_array()?;
                // Other servers list models here too, but only vLLM reports `max_model_len`.
                if !items.iter().any(|item| item.get("max_model_len").is_some()) {
                    return None;
                }
                Some(
                    items
                        .iter()
                        .find(|item| item.get("id").and_then(|v| v.as_str()) == Some(model))
                        .and_then(|item| item.get("max_model_len")?.as_u64()),
                )
            }
            ServerKind::LmStudio => {
                let origin = origin?;
                let mut url = Url::parse(origin).ok()?;
                url.path_segments_mut()
                    .ok()?
                    .pop_if_empty()
                    .extend(["api", "v0", "models", model]);
                // Some versions do not route an id containing `/`; the list always works.
                let item = match fetch_json(url.as_str()).await {
                    Ok(item) => item,
                    Err(_) => fetch_json(&format!("{origin}/api/v0/models"))
                        .await
                        .ok()?
                        .get("data")?
                        .as_array()?
                        .iter()
                        .find(|item| item.get("id").and_then(|v| v.as_str()) == Some(model))?
                        .clone(),
                };
                item.get("state")?.as_str()?;
                Some(item.get("loaded_context_length").and_then(|v| v.as_u64()))
            }
            ServerKind::Unknown => None,
        }
    };
    tokio::time::timeout(Duration::from_secs(3), probe)
        .await
        .ok()
        .flatten()
}

/// Whether requests may probe `endpoint` on their own. A cloud service over HTTPS is not asked;
/// it keeps the configured budget, and Settings can still detect it on demand.
fn probed_automatically(endpoint: &Url) -> bool {
    endpoint.scheme() != "https"
        || endpoint.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

/// Ask `endpoint` which server it is, given what this session already knows.
async fn identify(
    endpoint: &str,
    model: &str,
    cached: Option<ServerKind>,
) -> (ServerKind, Option<u64>) {
    let origin = endpoint.strip_suffix("/v1");
    let kinds = match cached {
        Some(ServerKind::Unknown) => return (ServerKind::Unknown, None),
        Some(kind) => vec![kind],
        None => ServerKind::PROBED.to_vec(),
    };
    let answers = futures_util::future::join_all(
        kinds
            .iter()
            .map(|kind| probe_kind(*kind, endpoint, origin, model)),
    )
    .await;
    let found = kinds
        .into_iter()
        .zip(answers)
        .find_map(|(kind, answer)| answer.map(|window| (kind, window)));
    match (found, cached) {
        (Some(found), _) => found,
        // A known server that failed to answer once is still that server.
        (None, Some(kind)) => (kind, None),
        (None, None) => (ServerKind::Unknown, None),
    }
}

fn remember(endpoint: &str, kind: ServerKind) {
    if let Ok(mut known) = known_kinds().lock() {
        known.insert(endpoint.to_owned(), kind);
    }
}

/// Find which server answers at `endpoint` and the context window it serves `model` with.
/// Every kind is asked at once, 3 s each, without credentials; when several answer, the first
/// in Ollama, llama.cpp, vLLM, LM Studio order wins. The kind, including unknown, is remembered
/// for the session, so later calls ask only that server's route, or nothing for an unknown one.
/// HTTPS hosts other than this computer are not asked; see [`detect_server`].
pub async fn probe_server(endpoint: &Url, model: &str) -> (ServerKind, Option<u64>) {
    if !probed_automatically(endpoint) {
        return (ServerKind::Unknown, None);
    }
    let endpoint = endpoint.as_str().trim_end_matches('/');
    let cached = known_kinds()
        .lock()
        .ok()
        .and_then(|kinds| kinds.get(endpoint).copied());
    let (kind, window) = identify(endpoint, model, cached).await;
    remember(endpoint, kind);
    (kind, window)
}

/// The on-demand Settings check: forget what this session knew about `endpoint` and ask every
/// kind again, cloud HTTPS hosts included.
pub async fn detect_server(endpoint: &Url, model: &str) -> (ServerKind, Option<u64>) {
    let endpoint = endpoint.as_str().trim_end_matches('/');
    let (kind, window) = identify(endpoint, model, None).await;
    remember(endpoint, kind);
    (kind, window)
}

/// [`probe_server`] for an endpoint string; an address that does not parse is unknown.
pub async fn probe_endpoint(endpoint: &str, model: &str) -> (ServerKind, Option<u64>) {
    match Url::parse(endpoint) {
        Ok(url) => probe_server(&url, model).await,
        Err(_) => (ServerKind::Unknown, None),
    }
}

/// [`detect_server`] for an endpoint string; an address that does not parse is unknown.
pub async fn detect_endpoint(endpoint: &str, model: &str) -> (ServerKind, Option<u64>) {
    match Url::parse(endpoint) {
        Ok(url) => detect_server(&url, model).await,
        Err(_) => (ServerKind::Unknown, None),
    }
}

/// Size `budget` to the context window the server loaded `model` with. OpenAI-compatible
/// routes often ignore context options, so the server's own report is the limit that applies.
/// Unknown services, unloaded models, and unreachable hosts keep the configured budget.
pub async fn served_budget(endpoint: &str, model: &str, budget: ContextBudget) -> ContextBudget {
    match probe_endpoint(endpoint, model).await {
        (_, Some(window)) => sized_to_window(budget, window),
        _ => budget,
    }
}

/// The window Ollama's `/api/ps` reports for `model`.
fn ollama_window(value: &serde_json::Value, model: &str) -> Option<u64> {
    // Ollama reports untagged names with their implied `:latest` tag.
    let tagged = if model.contains(':') {
        model.to_owned()
    } else {
        format!("{model}:latest")
    };
    value
        .get("models")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .find(|item| item.get("name").and_then(|v| v.as_str()) == Some(tagged.as_str()))
        .and_then(|item| item.get("context_length"))
        .and_then(|v| v.as_u64())
}

fn sized_to_window(budget: ContextBudget, window: u64) -> ContextBudget {
    let sized = ContextBudget {
        context_window: window.min(1_000_000),
        output_tokens: (window / 4).min(32_768) as u32,
        ..budget
    };
    if sized.validate().is_ok() {
        sized
    } else {
        budget
    }
}

/// Bounded, non-empty model names in service order. Ollama 0.40 can list one name once per
/// runner, so only the first occurrence is kept.
fn model_names(items: &[serde_json::Value], ollama: bool) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter_map(|item| {
            item.get(if ollama { "name" } else { "id" })
                .and_then(|v| v.as_str())
        })
        .filter(|name| !name.trim().is_empty() && name.len() <= 256 && seen.insert(*name))
        .take(200)
        .map(str::to_owned)
        .collect()
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
    fn sized(ps: &serde_json::Value, model: &str) -> ContextBudget {
        let configured = ContextBudget::default();
        ollama_window(ps, model).map_or(configured, |window| sized_to_window(configured, window))
    }
    #[test]
    fn budgets_follow_the_loaded_context_window() {
        let configured = ContextBudget::default();
        let ps = serde_json::json!({"models": [
            {"name": "other:latest", "context_length": 8192},
            {"name": "qwen3.6:35b", "context_length": 102_400}
        ]});
        let budget = sized(&ps, "qwen3.6:35b");
        assert_eq!(budget.context_window, 102_400);
        assert_eq!(budget.output_tokens, 25_600);
        assert_eq!(sized(&ps, "other:latest").output_tokens, 2048);
        for unusable in [
            serde_json::json!({}),
            serde_json::json!({"models": [{"name": "qwen3.6:35b"}]}),
            serde_json::json!({"models": [{"name": "qwen3.6:35b", "context_length": 512}]}),
        ] {
            assert_eq!(sized(&unusable, "qwen3.6:35b"), configured);
        }
        assert_eq!(sized(&ps, "missing"), configured);
        assert_eq!(sized(&ps, "other").context_window, 8192);
        let huge =
            serde_json::json!({"models": [{"name": "big:latest", "context_length": 2_000_000}]});
        assert_eq!(sized(&huge, "big").context_window, 1_000_000);
    }

    /// A fake answering only `routes`, each a path and its JSON body.
    async fn server(routes: &[(&str, serde_json::Value)]) -> crate::test_http::Fake {
        let routes: Vec<(String, String)> = routes
            .iter()
            .map(|(path, body)| ((*path).to_owned(), body.to_string()))
            .collect();
        crate::test_http::serve(move |method, path, _| {
            routes
                .iter()
                .find(|(route, _)| method == "GET" && route == path)
                .map(|(_, body)| (200, body.clone()))
        })
        .await
    }
    async fn probe(fake: &crate::test_http::Fake, model: &str) -> (ServerKind, Option<u64>) {
        probe_server(
            &Url::parse(&format!("{}/v1", fake.origin())).unwrap(),
            model,
        )
        .await
    }
    #[tokio::test]
    async fn probe_reads_llama_cpp_props_n_ctx() {
        let fake = server(&[
            (
                "/props",
                serde_json::json!({"default_generation_settings": {"n_ctx": 131_072}}),
            ),
            (
                "/v1/models",
                serde_json::json!({"data": [{"id": "qwen", "meta": {"n_ctx_train": 262_144}}]}),
            ),
        ])
        .await;
        assert_eq!(
            probe(&fake, "qwen").await,
            (ServerKind::LlamaCpp, Some(131_072))
        );
        let budget = served_budget(
            &format!("{}/v1", fake.origin()),
            "qwen",
            ContextBudget::default(),
        )
        .await;
        assert_eq!(
            (budget.context_window, budget.output_tokens),
            (131_072, 32_768)
        );
    }
    #[tokio::test]
    async fn probe_reads_vllm_max_model_len() {
        let fake = server(&[(
            "/v1/models",
            serde_json::json!({"data": [
                {"id": "other", "max_model_len": 4096},
                {"id": "Qwen/Qwen3-8B", "max_model_len": 40_960}
            ]}),
        )])
        .await;
        assert_eq!(
            probe(&fake, "Qwen/Qwen3-8B").await,
            (ServerKind::Vllm, Some(40_960))
        );
        assert_eq!(probe(&fake, "missing").await, (ServerKind::Vllm, None));
    }
    #[tokio::test]
    async fn probe_reads_lm_studio_loaded_context_length() {
        let fake = server(&[
            (
                "/api/v0/models/qwen%2Fqwen3-8b",
                serde_json::json!({"id": "qwen/qwen3-8b", "state": "loaded", "max_context_length": 40_960, "loaded_context_length": 16_384}),
            ),
            (
                "/v1/models",
                serde_json::json!({"data": [{"id": "qwen/qwen3-8b"}]}),
            ),
        ])
        .await;
        assert_eq!(
            probe(&fake, "qwen/qwen3-8b").await,
            (ServerKind::LmStudio, Some(16_384))
        );
    }
    #[tokio::test]
    async fn probe_ollama_on_custom_port() {
        let fake = server(&[
            ("/api/version", serde_json::json!({"version": "0.40.1"})),
            (
                "/api/ps",
                serde_json::json!({"models": [{"name": "qwen3.6:35b", "context_length": 65_536}]}),
            ),
            (
                "/v1/models",
                serde_json::json!({"data": [{"id": "qwen3.6:35b"}]}),
            ),
        ])
        .await;
        assert_ne!(fake.addr.port(), 11434);
        assert_eq!(
            probe(&fake, "qwen3.6:35b").await,
            (ServerKind::Ollama, Some(65_536))
        );
        // A model that is not loaded is still on Ollama, with no window to report.
        assert_eq!(probe(&fake, "llama3.2").await, (ServerKind::Ollama, None));
    }
    #[tokio::test]
    async fn probe_unknown_keeps_configured_budget() {
        let fake = server(&[("/v1/models", serde_json::json!({"data": [{"id": "gpt"}]}))]).await;
        assert_eq!(probe(&fake, "gpt").await, (ServerKind::Unknown, None));
        let configured = ContextBudget {
            context_window: 50_000,
            ..ContextBudget::default()
        };
        let endpoint = format!("{}/v1", fake.origin());
        assert_eq!(
            served_budget(&endpoint, "gpt", configured).await,
            configured
        );
        // Nothing listening at all keeps the budget too.
        drop(fake);
        assert_eq!(
            served_budget("http://127.0.0.1:9/v1", "gpt", configured).await,
            configured
        );
    }
    #[tokio::test]
    async fn unknown_server_is_probed_once_per_session() {
        let fake = server(&[("/v1/models", serde_json::json!({"data": [{"id": "m"}]}))]).await;
        assert_eq!(probe(&fake, "m").await, (ServerKind::Unknown, None));
        let first = fake.requests().len();
        assert!(first >= 3, "{:?}", fake.requests());
        assert_eq!(probe(&fake, "m").await, (ServerKind::Unknown, None));
        assert_eq!(fake.requests().len(), first);
        // Detecting on demand asks again.
        let endpoint = format!("{}/v1", fake.origin());
        assert_eq!(detect_endpoint(&endpoint, "m").await.0, ServerKind::Unknown);
        assert!(fake.requests().len() > first);
    }
    #[tokio::test]
    async fn cloud_https_host_is_not_probed() {
        for (address, probed) in [
            ("https://api.example.com/v1", false),
            ("https://10.0.0.5:8443/v1", false),
            ("https://localhost:8443/v1", true),
            ("https://127.0.0.1:8443/v1", true),
            ("https://[::1]:8443/v1", true),
            ("http://gpu-box:11434/v1", true),
        ] {
            assert_eq!(
                probed_automatically(&Url::parse(address).unwrap()),
                probed,
                "{address}"
            );
        }
        let configured = ContextBudget::default();
        let started = std::time::Instant::now();
        assert_eq!(
            served_budget("https://api.invalid/v1", "m", configured).await,
            configured
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(known_kinds()
            .lock()
            .unwrap()
            .get("https://api.invalid/v1")
            .is_none());
    }
    #[tokio::test]
    async fn known_kind_survives_a_transient_probe_failure() {
        let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let serving = up.clone();
        let fake = crate::test_http::serve(move |_, path, _| {
            (path == "/props" && serving.load(std::sync::atomic::Ordering::SeqCst)).then(|| {
                (
                    200,
                    serde_json::json!({"default_generation_settings": {"n_ctx": 4096}}).to_string(),
                )
            })
        })
        .await;
        assert_eq!(probe(&fake, "m").await, (ServerKind::LlamaCpp, Some(4096)));
        up.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(probe(&fake, "m").await, (ServerKind::LlamaCpp, None));
        up.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(probe(&fake, "m").await, (ServerKind::LlamaCpp, Some(4096)));
    }
    #[tokio::test]
    async fn lm_studio_ids_with_a_slash_fall_back_to_the_model_list() {
        let fake = server(&[(
            "/api/v0/models",
            serde_json::json!({"data": [
                {"id": "other", "state": "not-loaded", "max_context_length": 4096},
                {"id": "qwen/qwen3-8b", "state": "loaded", "max_context_length": 40_960, "loaded_context_length": 8192}
            ]}),
        )])
        .await;
        assert_eq!(
            probe(&fake, "qwen/qwen3-8b").await,
            (ServerKind::LmStudio, Some(8192))
        );
        assert!(fake
            .requests()
            .contains(&"GET /api/v0/models/qwen%2Fqwen3-8b".to_owned()));
    }
    #[tokio::test]
    async fn probe_remembers_the_server_kind() {
        let fake = server(&[(
            "/props",
            serde_json::json!({"default_generation_settings": {"n_ctx": 8192}}),
        )])
        .await;
        assert_eq!(probe(&fake, "m").await.0, ServerKind::LlamaCpp);
        let first = fake.requests().len();
        assert!(first >= 3, "{:?}", fake.requests());
        assert_eq!(probe(&fake, "m").await, (ServerKind::LlamaCpp, Some(8192)));
        assert_eq!(fake.requests()[first..], ["GET /props".to_owned()]);
    }
    #[test]
    fn discovered_model_names_are_listed_once() {
        let tags = [
            serde_json::json!({"name": "qwen3.6:latest"}),
            serde_json::json!({"name": "qwen3.6:latest"}),
            serde_json::json!({"name": " "}),
            serde_json::json!({"name": "llama3.2:1b"}),
        ];
        assert_eq!(model_names(&tags, true), ["qwen3.6:latest", "llama3.2:1b"]);
        assert_eq!(
            model_names(
                &[
                    serde_json::json!({"id": "a"}),
                    serde_json::json!({"id": "a"})
                ],
                false
            ),
            ["a"]
        );
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
