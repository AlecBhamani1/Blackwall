//! Optional OTLP/HTTP export of the same finished, scrubbed runs and metric lines that are written
//! to the local files.
//!
//! Two small exporters POST official `opentelemetry-proto` messages, as JSON or protobuf, to
//! `{endpoint}/v1/traces` and `{endpoint}/v1/metrics` with the workspace reqwest client: redirects
//! are never followed and every request has a 10 s timeout. Nothing here reads an `OTEL_*`
//! environment variable. A dedicated thread does the sending, fed by a bounded queue (2,048
//! spans): a full queue drops and counts, so export never blocks or fails a run.
//!
//! Header values (such as an API key) live only in the credential store, service
//! [`SECRET_SERVICE`], account `{origin}|{name}`: each value is bound to the endpoint origin
//! (`scheme://host:port`) it was entered for, and is only ever read for that origin. Settings
//! files hold only each header's name and origin.
use super::names;
use opentelemetry_proto::tonic::{
    collector::{metrics::v1::ExportMetricsServiceRequest, trace::v1::ExportTraceServiceRequest},
    common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue},
    metrics::v1::{metric::Data, number_data_point, HistogramDataPoint, NumberDataPoint},
    resource::v1::Resource,
    trace::v1::{span::SpanKind, status::StatusCode, ResourceSpans, ScopeSpans, Span, Status},
};
use prost::Message as _;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE},
    redirect::Policy,
    Url,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The credential-store service that holds export header values.
pub const SECRET_SERVICE: &str = "com.blackwall.telemetry";
/// Spans waiting to be sent; more are dropped and counted.
pub const QUEUE_SPANS: usize = 2048;
/// Metric requests waiting to be sent (one per minute at most); more are dropped and counted.
const QUEUE_METRICS: usize = 64;
/// Every export request, including the connection, must finish within this.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// How long shutdown waits for queued data to be sent.
pub(super) const DRAIN: Duration = Duration::from_secs(3);
const MAX_HEADERS: usize = 16;
const MAX_HEADER_VALUE: usize = 8192;
const MAX_ENDPOINT: usize = 2048;
/// Header names the exporter sets itself or that HTTP reserves.
const RESERVED_HEADERS: [&str; 7] = [
    "content-type",
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "host",
    "te",
];

/// The OTLP/HTTP encoding.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Protocol {
    #[serde(rename = "http/json")]
    HttpJson,
    /// The OTLP/HTTP default.
    #[default]
    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HttpJson => "http/json",
            Self::HttpProtobuf => "http/protobuf",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::HttpJson => "application/json",
            Self::HttpProtobuf => "application/x-protobuf",
        }
    }
}

/// The owner's export settings. Off by default: there is no endpoint until the owner sets one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ExportConfig {
    /// The OTLP/HTTP base URL; `/v1/traces` and `/v1/metrics` are added to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub protocol: Protocol,
    pub traces: bool,
    pub metrics: bool,
    /// Header names and the origin each was saved for; values are in the credential store.
    pub headers: Vec<SavedHeader>,
    /// Keep `blackwall.project` and `blackwall.session` on exported metrics.
    pub include_project: bool,
}

/// One export header: its lower-case name and the endpoint origin its value was saved for.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SavedHeader {
    pub name: String,
    pub origin: String,
}

/// Saved export fields; anything else in a saved settings object is ignored, except a field that
/// could hold a header value, which is refused.
const SAVED_FIELDS: [&str; 6] = [
    "endpoint",
    "protocol",
    "traces",
    "metrics",
    "headers",
    "includeProject",
];

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            protocol: Protocol::default(),
            traces: true,
            metrics: true,
            headers: vec![],
            include_project: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Signal {
    Traces,
    Metrics,
}

impl Signal {
    fn path(self) -> &'static str {
        match self {
            Self::Traces => "v1/traces",
            Self::Metrics => "v1/metrics",
        }
    }
}

impl ExportConfig {
    /// Whether anything would be sent.
    pub fn is_on(&self) -> bool {
        self.endpoint.is_some() && (self.traces || self.metrics)
    }

    /// Settings read back from a settings file. Fields written by a newer version are ignored,
    /// but a `headerValues` field (or any key mentioning a value or secret) is refused, so a
    /// header value can never be loaded from a file.
    pub fn from_saved(value: &serde_json::Value) -> Result<Self, ExportError> {
        let Some(object) = value.as_object() else {
            return Err(ExportError::InvalidSettings);
        };
        if object.keys().any(|key| {
            let key = key.to_ascii_lowercase();
            key.contains("value") || key.contains("secret")
        }) {
            return Err(ExportError::InvalidSettings);
        }
        let known: serde_json::Map<_, _> = object
            .iter()
            .filter(|(key, _)| SAVED_FIELDS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        serde_json::from_value(serde_json::Value::Object(known))
            .map_err(|_| ExportError::InvalidSettings)
    }

    /// The endpoint's origin, `scheme://host:port`, that header values are bound to.
    pub fn origin(&self) -> Result<String, ExportError> {
        origin_of(&normalize_endpoint(
            self.endpoint.as_deref().ok_or(ExportError::Off)?,
        )?)
    }

    /// The settings with a normalized endpoint and lower-case, de-duplicated headers, or the
    /// first problem found.
    pub fn validated(&self) -> Result<Self, ExportError> {
        let endpoint = self
            .endpoint
            .as_deref()
            .map(|text| normalize_endpoint(text).map(String::from))
            .transpose()?;
        let mut headers: Vec<SavedHeader> = vec![];
        for header in &self.headers {
            let header = SavedHeader {
                name: header_name(&header.name)?,
                origin: normalize_origin(&header.origin)?,
            };
            if !headers.contains(&header) {
                headers.push(header);
            }
        }
        if headers.len() > MAX_HEADERS {
            return Err(ExportError::TooManyHeaders);
        }
        Ok(Self {
            endpoint,
            headers,
            ..self.clone()
        })
    }

    fn signal_url(&self, signal: Signal) -> Result<Url, ExportError> {
        let mut url = normalize_endpoint(self.endpoint.as_deref().ok_or(ExportError::Off)?)?;
        let path = format!("{}/{}", url.path().trim_end_matches('/'), signal.path());
        url.set_path(&path);
        Ok(url)
    }
}

/// An endpoint as an OTLP/HTTP base URL. `https` is required, except plain `http` to the literal
/// loopback hosts `127.0.0.1`, `::1`, and `localhost`. Credentials, queries, and fragments are
/// refused so no secret is ever saved with the URL; a full `/v1/traces` or `/v1/metrics` path is
/// trimmed back to its base.
pub fn normalize_endpoint(text: &str) -> Result<Url, ExportError> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_ENDPOINT || text.chars().any(char::is_control) {
        return Err(ExportError::InvalidEndpoint);
    }
    let mut url = Url::parse(text).map_err(|_| ExportError::InvalidEndpoint)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none_or(str::is_empty)
    {
        return Err(ExportError::InvalidEndpoint);
    }
    match url.scheme() {
        "https" => {}
        "http" if matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")) => {}
        "http" => return Err(ExportError::InsecureEndpoint),
        _ => return Err(ExportError::InvalidEndpoint),
    }
    let path = url.path().trim_end_matches('/');
    let base = path
        .strip_suffix("/v1/traces")
        .or_else(|| path.strip_suffix("/v1/metrics"))
        .unwrap_or(path)
        .to_owned();
    url.set_path(&base);
    Ok(url)
}

/// `scheme://host:port` of a validated endpoint, with the port always written out.
fn origin_of(url: &Url) -> Result<String, ExportError> {
    let host = url.host_str().ok_or(ExportError::InvalidEndpoint)?;
    let port = url
        .port_or_known_default()
        .ok_or(ExportError::InvalidEndpoint)?;
    Ok(format!("{}://{host}:{port}", url.scheme()))
}

/// A saved origin, checked: it must be the origin of an endpoint export would accept.
pub fn normalize_origin(text: &str) -> Result<String, ExportError> {
    let origin = origin_of(&normalize_endpoint(text)?)?;
    if origin != text {
        return Err(ExportError::InvalidEndpoint);
    }
    Ok(origin)
}

fn account(origin: &str, name: &str) -> String {
    format!("{origin}|{name}")
}

/// A header name as saved: lower case letters, digits, and hyphens, not one the exporter sets.
pub fn header_name(name: &str) -> Result<String, ExportError> {
    let name = name.trim().to_ascii_lowercase();
    let plain = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !plain || RESERVED_HEADERS.contains(&name.as_str()) {
        return Err(ExportError::InvalidHeader);
    }
    Ok(name)
}

fn header_value(value: &str) -> Result<HeaderValue, ExportError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_HEADER_VALUE {
        return Err(ExportError::InvalidHeaderValue);
    }
    let mut value = HeaderValue::from_str(value).map_err(|_| ExportError::InvalidHeaderValue)?;
    value.set_sensitive(true);
    Ok(value)
}

/// Why an export setting was refused or a send failed. Messages never include a header value or a
/// response body.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExportError {
    #[error("Enter an OTLP endpoint URL without a user name, password, query, or fragment.")]
    InvalidEndpoint,
    #[error(
        "Use https for the OTLP endpoint. Plain http is allowed only for 127.0.0.1, ::1, or localhost."
    )]
    InsecureEndpoint,
    #[error("Header names use letters, digits, and hyphens (at most 64), and cannot be Content-Type, Host, or another header HTTP sets itself.")]
    InvalidHeader,
    #[error("Enter a header value of 1 to 8,192 visible characters.")]
    InvalidHeaderValue,
    #[error("Save at most 16 export headers.")]
    TooManyHeaders,
    #[error("The value of the {0} header is not in {store}. Add it again.", store = crate::secrets::STORE)]
    MissingHeader(String),
    #[error("The {0} header was saved for {1}, not this endpoint. Remove it or enter it again.")]
    HeaderOtherOrigin(String, String),
    #[error("The saved export settings are invalid.")]
    InvalidSettings,
    #[error(
        "{} could not be read. {}, then try again.",
        crate::secrets::STORE,
        crate::secrets::RECOVER
    )]
    CredentialStore,
    #[error("{}", crate::secrets::KEYRING_OFF_NOTE)]
    KeyringOff,
    #[error("Export is off. Set an OTLP endpoint first.")]
    Off,
    #[error("The collector could not be reached.")]
    Connect,
    #[error("The collector did not answer within 10 seconds.")]
    Timeout,
    #[error("The collector answered with a redirect (HTTP {0}). Redirects are not followed; enter the final URL.")]
    Redirect(u16),
    #[error("The collector answered HTTP {0}.")]
    Status(u16),
    #[error("The export request failed.")]
    Request,
}

impl ExportError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidEndpoint => "invalid_endpoint",
            Self::InsecureEndpoint => "insecure_endpoint",
            Self::InvalidHeader => "invalid_header",
            Self::InvalidHeaderValue => "invalid_header_value",
            Self::TooManyHeaders => "too_many_headers",
            Self::MissingHeader(_) => "missing_header",
            Self::HeaderOtherOrigin(..) => "header_other_origin",
            Self::InvalidSettings => "invalid_settings",
            Self::CredentialStore => "credential_store",
            Self::KeyringOff => "keyring_off",
            Self::Off => "off",
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Redirect(_) => "redirect",
            Self::Status(_) => "http_status",
            Self::Request => "request",
        }
    }

    /// The HTTP status the collector answered with, if it answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Redirect(status) | Self::Status(status) => Some(*status),
            _ => None,
        }
    }
}

/// Reads one header value by its credential-store account, `{origin}|{name}`.
pub type HeaderLookup<'a> =
    dyn Fn(&str) -> Result<Option<String>, crate::secrets::SecretError> + 'a;

/// A header value from the credential store, by account. Blocking.
pub fn keychain_header(account: &str) -> Result<Option<String>, crate::secrets::SecretError> {
    crate::secrets::get(SECRET_SERVICE, account)
}

/// Save a header value for the origin of `endpoint`. Blocking. Returns the header to list in the
/// settings.
pub fn save_header(endpoint: &str, name: &str, value: &str) -> Result<SavedHeader, ExportError> {
    save_header_with(endpoint, name, value, crate::secrets::set)
}

/// [`save_header`] with the secret write supplied by the caller.
pub fn save_header_with(
    endpoint: &str,
    name: &str,
    value: &str,
    set: crate::secrets::SetSecret,
) -> Result<SavedHeader, ExportError> {
    let origin = origin_of(&normalize_endpoint(endpoint)?)?;
    let name = header_name(name)?;
    let value = header_value(value)?;
    let text = value
        .to_str()
        .map_err(|_| ExportError::InvalidHeaderValue)?;
    set(SECRET_SERVICE, &account(&origin, &name), text).map_err(|error| match error {
        crate::secrets::SecretError::Unavailable => ExportError::KeyringOff,
        _ => ExportError::CredentialStore,
    })?;
    Ok(SavedHeader { name, origin })
}

/// Remove a saved header value. Blocking; a missing value succeeds.
pub fn delete_header(header: &SavedHeader) -> Result<(), ExportError> {
    let name = header_name(&header.name)?;
    let origin = normalize_origin(&header.origin)?;
    crate::secrets::delete(SECRET_SERVICE, &account(&origin, &name))
        .map_err(|_| ExportError::CredentialStore)
}

/// Every configured header with its value from `lookup`, read only for the endpoint's own
/// origin. A header saved for another origin refuses the settings: its value is never sent to a
/// different endpoint. Blocking when `lookup` is.
pub fn resolve_headers(
    config: &ExportConfig,
    lookup: &HeaderLookup<'_>,
) -> Result<HeaderMap, ExportError> {
    let origin = config.origin()?;
    let mut headers = HeaderMap::new();
    for header in &config.headers {
        let name = header_name(&header.name)?;
        if header.origin != origin {
            return Err(ExportError::HeaderOtherOrigin(name, header.origin.clone()));
        }
        let value = lookup(&account(&origin, &name))
            .map_err(|_| ExportError::CredentialStore)?
            .ok_or_else(|| ExportError::MissingHeader(name.clone()))?;
        let key =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| ExportError::InvalidHeader)?;
        headers.insert(key, header_value(&value)?);
    }
    Ok(headers)
}

/// Headers for a test span: only those saved for the endpoint's own origin; others are left
/// out, so a draft endpoint never receives another origin's values.
pub fn resolve_test_headers(
    config: &ExportConfig,
    lookup: &HeaderLookup<'_>,
) -> Result<HeaderMap, ExportError> {
    let origin = config.origin()?;
    let mut matching = config.clone();
    matching.headers.retain(|header| header.origin == origin);
    resolve_headers(&matching, lookup)
}

/// The HTTP client for export: no redirects, a 10 s limit per request.
fn client() -> Result<reqwest::Client, ExportError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(TIMEOUT)
        .connect_timeout(TIMEOUT)
        .build()
        .map_err(|_| ExportError::Request)
}

fn classify(error: &reqwest::Error) -> ExportError {
    if error.is_timeout() {
        ExportError::Timeout
    } else if error.is_connect() {
        ExportError::Connect
    } else {
        ExportError::Request
    }
}

/// POST one encoded message; the response body is never read.
async fn post(
    client: &reqwest::Client,
    url: Url,
    headers: &HeaderMap,
    protocol: Protocol,
    body: Vec<u8>,
) -> Result<u16, ExportError> {
    let response = client
        .post(url)
        .headers(headers.clone())
        .header(CONTENT_TYPE, protocol.content_type())
        .body(body)
        .send()
        .await
        .map_err(|error| classify(&error))?;
    let status = response.status();
    drop(response);
    if status.is_success() {
        Ok(status.as_u16())
    } else if status.is_redirection() {
        Err(ExportError::Redirect(status.as_u16()))
    } else {
        Err(ExportError::Status(status.as_u16()))
    }
}

fn encode_traces(protocol: Protocol, request: &ExportTraceServiceRequest) -> Option<Vec<u8>> {
    match protocol {
        Protocol::HttpJson => serde_json::to_vec(request).ok(),
        Protocol::HttpProtobuf => Some(request.encode_to_vec()),
    }
}

fn encode_metrics(protocol: Protocol, request: &ExportMetricsServiceRequest) -> Option<Vec<u8>> {
    match protocol {
        Protocol::HttpJson => serde_json::to_vec(request).ok(),
        Protocol::HttpProtobuf => Some(request.encode_to_vec()),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn string(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.into())),
        }),
        ..Default::default()
    }
}

/// One span named `blackwall test span`, with the same resource as real runs.
fn test_request(surface: &str) -> ExportTraceServiceRequest {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let span = Span {
        trace_id: rand::random::<[u8; 16]>().to_vec(),
        span_id: rand::random::<[u8; 8]>().to_vec(),
        name: "blackwall test span".into(),
        kind: SpanKind::Internal as i32,
        start_time_unix_nano: now.saturating_sub(1_000_000),
        end_time_unix_nano: now,
        status: Some(Status {
            code: StatusCode::Ok as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![
                    string(names::SERVICE_NAME, "blackwall"),
                    string(names::SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
                    string(names::SURFACE, surface),
                ],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "blackwall".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    ..Default::default()
                }),
                spans: vec![span],
                schema_url: names::SCHEMA_URL.into(),
            }],
            schema_url: names::SCHEMA_URL.into(),
        }],
    }
}

/// Send one test span with header values already read. Returns the HTTP status.
pub async fn send_test_span_with(
    config: &ExportConfig,
    headers: &HeaderMap,
    surface: &str,
) -> Result<u16, ExportError> {
    let config = config.validated()?;
    let url = config.signal_url(Signal::Traces)?;
    let body =
        encode_traces(config.protocol, &test_request(surface)).ok_or(ExportError::Request)?;
    post(&client()?, url, headers, config.protocol, body).await
}

/// Send one test span with saved settings, reading header values from the credential store the
/// same way export does: a header saved for another origin fails the test, as it pauses export.
/// Returns the HTTP status.
pub async fn send_test_span(config: &ExportConfig, surface: &str) -> Result<u16, ExportError> {
    let validated = config.validated()?;
    let lookup = validated.clone();
    let headers = tokio::task::spawn_blocking(move || resolve_headers(&lookup, &keychain_header))
        .await
        .map_err(|_| ExportError::CredentialStore)??;
    send_test_span_with(&validated, &headers, surface).await
}

/// The last export outcome. Nothing in it comes from a response body.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportFailure {
    pub kind: &'static str,
    pub status: Option<u16>,
    /// `traces`, `metrics`, or `test`.
    pub signal: &'static str,
    pub unix_ms: u64,
}

/// What the export has done since this process started.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportStatus {
    pub endpoint: Option<String>,
    pub protocol: Protocol,
    pub traces: bool,
    pub metrics: bool,
    pub last_success_unix_ms: Option<u64>,
    pub last_error: Option<ExportFailure>,
    pub sent_spans: u64,
    pub dropped_spans: u64,
    pub sent_metrics: u64,
    pub dropped_metrics: u64,
}

impl ExportStatus {
    fn record(&mut self, signal: &'static str, result: &Result<u16, ExportError>) {
        match result {
            Ok(_) => self.last_success_unix_ms = Some(now_ms()),
            Err(error) => {
                self.last_error = Some(ExportFailure {
                    kind: error.kind(),
                    status: error.status(),
                    signal,
                    unix_ms: now_ms(),
                })
            }
        }
    }
}

/// Where and how to send, with header values read once when the settings were applied.
#[derive(Debug)]
struct Target {
    config: ExportConfig,
    traces: Option<Url>,
    metrics: Option<Url>,
    headers: HeaderMap,
    client: reqwest::Client,
}

enum Job {
    Spans(ExportTraceServiceRequest, usize),
    Metrics(ExportMetricsServiceRequest),
    Flush(mpsc::SyncSender<()>),
}

#[derive(Default)]
struct State {
    target: Mutex<Option<Arc<Target>>>,
    status: Mutex<ExportStatus>,
    queued_spans: AtomicUsize,
    queued_metrics: AtomicUsize,
}

impl State {
    fn target(&self) -> Option<Arc<Target>> {
        self.target.lock().ok()?.clone()
    }

    fn status(&self, change: impl FnOnce(&mut ExportStatus)) {
        if let Ok(mut status) = self.status.lock() {
            change(&mut status);
        }
    }

    fn send(&self, runtime: &tokio::runtime::Runtime, job: Job) {
        match job {
            Job::Spans(request, count) => {
                let result = self.target().and_then(|target| {
                    let url = target.traces.clone()?;
                    let body = encode_traces(target.config.protocol, &request)?;
                    Some(runtime.block_on(post(
                        &target.client,
                        url,
                        &target.headers,
                        target.config.protocol,
                        body,
                    )))
                });
                self.queued_spans.fetch_sub(count, Ordering::Relaxed);
                self.status(|status| match &result {
                    Some(Ok(_)) => status.sent_spans += count as u64,
                    _ => status.dropped_spans += count as u64,
                });
                if let Some(result) = result {
                    self.status(|status| status.record("traces", &result));
                }
            }
            Job::Metrics(mut request) => {
                let result = self.target().and_then(|target| {
                    let url = target.metrics.clone()?;
                    if !target.config.include_project {
                        strip_labels(&mut request);
                    }
                    let body = encode_metrics(target.config.protocol, &request)?;
                    Some(runtime.block_on(post(
                        &target.client,
                        url,
                        &target.headers,
                        target.config.protocol,
                        body,
                    )))
                });
                self.queued_metrics.fetch_sub(1, Ordering::Relaxed);
                self.status(|status| match &result {
                    Some(Ok(_)) => status.sent_metrics += 1,
                    _ => status.dropped_metrics += 1,
                });
                if let Some(result) = result {
                    self.status(|status| status.record("metrics", &result));
                }
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// The OTLP side of the run buffer and the metric reader. Cheap to clone; the sending thread
/// starts the first time export is turned on and stops when every clone is gone.
#[derive(Clone, Default)]
pub(super) struct Exporter {
    state: Arc<State>,
    queue: Arc<Mutex<Option<mpsc::SyncSender<Job>>>>,
}

impl std::fmt::Debug for Exporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Exporter")
    }
}

impl Exporter {
    /// Apply settings already validated, with their header values. `None` turns export off.
    pub(super) fn configure(&self, settings: Option<(ExportConfig, HeaderMap)>) {
        let target = settings.and_then(|(config, headers)| {
            let traces = config
                .traces
                .then(|| config.signal_url(Signal::Traces).ok())
                .flatten();
            let metrics = config
                .metrics
                .then(|| config.signal_url(Signal::Metrics).ok())
                .flatten();
            let client = client().ok()?;
            Some(Arc::new(Target {
                config,
                traces,
                metrics,
                headers,
                client,
            }))
        });
        self.state.status(|status| {
            let config = target.as_ref().map(|target| &target.config);
            status.endpoint = config.and_then(|config| config.endpoint.clone());
            status.protocol = config.map(|config| config.protocol).unwrap_or_default();
            status.traces = config.is_some_and(|config| config.traces);
            status.metrics = config.is_some_and(|config| config.metrics);
            status.last_error = None;
        });
        let on = target.is_some();
        if let Ok(mut current) = self.state.target.lock() {
            *current = target;
        }
        if on {
            self.start();
        }
    }

    /// Record a failure to apply settings, such as a missing header value, and turn export off.
    pub(super) fn refuse(&self, config: &ExportConfig, error: &ExportError) {
        self.configure(None);
        self.state.status(|status| {
            // Only a validated endpoint is shown; a refused one may not be a clean URL.
            status.endpoint = config.validated().ok().and_then(|config| config.endpoint);
            status.protocol = config.protocol;
            status.record("traces", &Err(error.clone()));
        });
    }

    fn start(&self) {
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        if queue.is_some() {
            return;
        }
        let (sender, jobs) = mpsc::sync_channel::<Job>(QUEUE_SPANS + QUEUE_METRICS + 8);
        let state = self.state.clone();
        let spawned = std::thread::Builder::new()
            .name("blackwall-otlp".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                for job in jobs {
                    state.send(&runtime, job);
                }
            });
        if spawned.is_ok() {
            *queue = Some(sender);
        }
    }

    fn sender(&self) -> Option<mpsc::SyncSender<Job>> {
        self.queue.lock().ok()?.clone()
    }

    /// Queue one finished, scrubbed run without blocking. A full queue drops it and counts.
    pub(super) fn submit_spans(&self, request: &ExportTraceServiceRequest) {
        if !self
            .state
            .target()
            .is_some_and(|target| target.traces.is_some())
        {
            return;
        }
        let count: usize = request
            .resource_spans
            .iter()
            .flat_map(|resource| &resource.scope_spans)
            .map(|scope| scope.spans.len())
            .sum();
        let queued = self.state.queued_spans.fetch_add(count, Ordering::Relaxed);
        let sent = queued + count <= QUEUE_SPANS
            && self
                .sender()
                .is_some_and(|queue| queue.try_send(Job::Spans(request.clone(), count)).is_ok());
        if !sent {
            self.state.queued_spans.fetch_sub(count, Ordering::Relaxed);
            self.state
                .status(|status| status.dropped_spans += count as u64);
        }
    }

    /// Queue one metrics request (the line the file exporter wrote) without blocking.
    pub(super) fn submit_metrics(&self, request: &ExportMetricsServiceRequest) {
        if !self
            .state
            .target()
            .is_some_and(|target| target.metrics.is_some())
        {
            return;
        }
        let queued = self.state.queued_metrics.fetch_add(1, Ordering::Relaxed);
        let sent = queued < QUEUE_METRICS
            && self
                .sender()
                .is_some_and(|queue| queue.try_send(Job::Metrics(request.clone())).is_ok());
        if !sent {
            self.state.queued_metrics.fetch_sub(1, Ordering::Relaxed);
            self.state.status(|status| status.dropped_metrics += 1);
        }
    }

    /// Wait up to `timeout` for everything queued so far to be sent.
    pub(super) fn drain(&self, timeout: Duration) {
        let Some(queue) = self.sender() else {
            return;
        };
        let (done, finished) = mpsc::sync_channel(1);
        if queue.try_send(Job::Flush(done)).is_ok() {
            let _ = finished.recv_timeout(timeout);
        }
    }

    /// Drain, then stop the sending thread.
    pub(super) fn stop(&self, timeout: Duration) {
        self.drain(timeout);
        if let Ok(mut queue) = self.queue.lock() {
            queue.take();
        }
    }

    pub(super) fn status(&self) -> ExportStatus {
        self.state
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    pub(super) fn record_test(&self, result: &Result<u16, ExportError>) {
        self.state.status(|status| status.record("test", result));
    }
}

fn is_label(pair: &KeyValue) -> bool {
    pair.key == names::PROJECT || pair.key == names::SESSION
}

/// Drop the labels and sort the rest by key, so equal attribute sets compare equal.
fn strip(attributes: &mut Vec<KeyValue>) {
    attributes.retain(|pair| !is_label(pair));
    attributes.sort_by(|a, b| a.key.cmp(&b.key));
}

fn strip_exemplars(exemplars: &mut [opentelemetry_proto::tonic::metrics::v1::Exemplar]) {
    for exemplar in exemplars {
        exemplar.filtered_attributes.retain(|pair| !is_label(pair));
    }
}

/// Remove `blackwall.project` and `blackwall.session` from every data point, then merge points
/// whose attributes became equal so each series appears once.
pub(super) fn strip_labels(request: &mut ExportMetricsServiceRequest) {
    for metric in request
        .resource_metrics
        .iter_mut()
        .flat_map(|resource| &mut resource.scope_metrics)
        .flat_map(|scope| &mut scope.metrics)
    {
        match &mut metric.data {
            Some(Data::Sum(sum)) => {
                let points = std::mem::take(&mut sum.data_points);
                sum.data_points = merge_numbers(points);
            }
            Some(Data::Histogram(histogram)) => {
                let points = std::mem::take(&mut histogram.data_points);
                histogram.data_points = merge_histograms(points);
            }
            Some(Data::Gauge(gauge)) => {
                for point in &mut gauge.data_points {
                    point.attributes.retain(|pair| !is_label(pair));
                    strip_exemplars(&mut point.exemplars);
                }
            }
            Some(Data::ExponentialHistogram(histogram)) => {
                for point in &mut histogram.data_points {
                    point.attributes.retain(|pair| !is_label(pair));
                    strip_exemplars(&mut point.exemplars);
                }
            }
            Some(Data::Summary(summary)) => {
                for point in &mut summary.data_points {
                    point.attributes.retain(|pair| !is_label(pair));
                }
            }
            None => {}
        }
    }
}

fn merge_numbers(points: Vec<NumberDataPoint>) -> Vec<NumberDataPoint> {
    use number_data_point::Value;
    let mut merged: Vec<NumberDataPoint> = vec![];
    for mut point in points {
        strip(&mut point.attributes);
        strip_exemplars(&mut point.exemplars);
        let same = merged.iter_mut().find(|other| {
            other.attributes == point.attributes
                && matches!(
                    (&other.value, &point.value),
                    (Some(Value::AsInt(_)), Some(Value::AsInt(_)))
                        | (Some(Value::AsDouble(_)), Some(Value::AsDouble(_)))
                )
        });
        let Some(other) = same else {
            merged.push(point);
            continue;
        };
        other.value = match (other.value.take(), point.value) {
            (Some(Value::AsInt(a)), Some(Value::AsInt(b))) => {
                Some(Value::AsInt(a.saturating_add(b)))
            }
            (Some(Value::AsDouble(a)), Some(Value::AsDouble(b))) => Some(Value::AsDouble(a + b)),
            (value, _) => value,
        };
        other.start_time_unix_nano = other.start_time_unix_nano.min(point.start_time_unix_nano);
        other.time_unix_nano = other.time_unix_nano.max(point.time_unix_nano);
        other.exemplars.extend(point.exemplars);
    }
    merged
}

fn merge_histograms(points: Vec<HistogramDataPoint>) -> Vec<HistogramDataPoint> {
    let mut merged: Vec<HistogramDataPoint> = vec![];
    for mut point in points {
        strip(&mut point.attributes);
        strip_exemplars(&mut point.exemplars);
        let same = merged.iter_mut().find(|other| {
            other.attributes == point.attributes
                && other.explicit_bounds == point.explicit_bounds
                && other.bucket_counts.len() == point.bucket_counts.len()
        });
        let Some(other) = same else {
            merged.push(point);
            continue;
        };
        other.count = other.count.saturating_add(point.count);
        other.sum = match (other.sum, point.sum) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        for (total, count) in other.bucket_counts.iter_mut().zip(&point.bucket_counts) {
            *total = total.saturating_add(*count);
        }
        other.min = match (other.min, point.min) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        other.max = match (other.max, point.max) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        other.start_time_unix_nano = other.start_time_unix_nano.min(point.start_time_unix_nano);
        other.time_unix_nano = other.time_unix_nano.max(point.time_unix_nano);
        other.exemplars.extend(point.exemplars);
    }
    merged
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
