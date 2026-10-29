//! Local traces and usage metrics through the OpenTelemetry SDK, using the GenAI semantic
//! conventions. Spans go to `traces/` as one OTLP/JSON file per run, and metrics go to
//! `metrics/YYYY-MM-DD.jsonl` as OTLP/JSON lines. Nothing is sent anywhere unless the owner sets
//! an OTLP endpoint ([`export`]); export then receives the same scrubbed data as the files.
//!
//! One [`Telemetry`] per process owns an `SdkTracerProvider` and an `SdkMeterProvider`. Each run
//! gets a [`RunScope`]. Timings, token counts, and outcomes are always recorded; prompts and
//! outputs only when the owner turns on content capture. Metrics never carry prompt text, tool
//! arguments, or paths. No `OTEL_*` environment variable changes this behaviour: every setting
//! the SDK would read from the environment is given explicitly.
pub mod export;
pub(crate) mod file_metrics;
mod file_spans;
pub mod names;
mod redact;

pub use export::{ExportConfig, ExportError, ExportStatus, Protocol};
pub use file_metrics::METRICS_DIRECTORY;

use crate::protocol::TokenUsage;
use file_spans::{RunBufferProcessor, RunState, Runs, MAX_SPANS};
use opentelemetry::{
    metrics::{Counter, Histogram, MeterProvider as _},
    trace::{
        Span as _, SpanContext, SpanKind, Status, TraceContextExt as _, TraceFlags, TraceState,
        Tracer as _, TracerProvider as _,
    },
    Array, Context, InstrumentationScope, KeyValue, StringValue,
};
use opentelemetry_sdk::{
    metrics::{PeriodicReader, SdkMeterProvider},
    trace::{Sampler, SdkTracer, SdkTracerProvider, SpanLimits},
    Resource,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How long usage metrics are kept. Days are clamped to 30–3,650. Saved as a number of days or
/// `"forever"`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "RetentionValue", into = "RetentionValue")]
pub enum Retention {
    Days(u16),
    Forever,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum RetentionValue {
    Days(u16),
    Text(String),
}

impl From<Retention> for RetentionValue {
    fn from(retention: Retention) -> Self {
        match retention {
            Retention::Days(days) => Self::Days(days),
            Retention::Forever => Self::Text("forever".into()),
        }
    }
}

impl TryFrom<RetentionValue> for Retention {
    type Error = String;

    fn try_from(value: RetentionValue) -> Result<Self, Self::Error> {
        match value {
            RetentionValue::Days(days) => Self::checked(days),
            RetentionValue::Text(text) => text.parse(),
        }
    }
}

impl std::str::FromStr for Retention {
    type Err = String;

    /// `forever` or a whole number of days from 30 to 3,650.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("forever") {
            return Ok(Self::Forever);
        }
        text.parse::<u16>()
            .map_err(|_| Self::RANGE.to_owned())
            .and_then(Self::checked)
    }
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Days(days) => write!(f, "{days} days"),
            Self::Forever => f.write_str("forever"),
        }
    }
}

impl Default for Retention {
    fn default() -> Self {
        Self::Days(400)
    }
}

impl Retention {
    pub const MIN_DAYS: u16 = 30;
    pub const MAX_DAYS: u16 = 3650;
    const RANGE: &'static str = "Keep usage history for 30 to 3,650 days, or forever.";

    /// Days within 30–3,650.
    pub fn checked(days: u16) -> Result<Self, String> {
        (Self::MIN_DAYS..=Self::MAX_DAYS)
            .contains(&days)
            .then_some(Self::Days(days))
            .ok_or_else(|| Self::RANGE.to_owned())
    }

    /// The kept days, or `None` to keep everything.
    pub fn days(self) -> Option<u16> {
        match self {
            Self::Days(days) => Some(days.clamp(Self::MIN_DAYS, Self::MAX_DAYS)),
            Self::Forever => None,
        }
    }
}

/// Which client recorded the data, as the `blackwall.surface` resource attribute.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Surface {
    #[default]
    Desktop,
    Cli,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Cli => "cli",
        }
    }
}

/// Process settings. `traces` turns all telemetry on (spans and usage metrics); `content` adds
/// redacted prompts, outputs, and tool data to spans. The default records nothing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TelemetryOptions {
    pub traces: bool,
    pub content: bool,
    pub retention: Retention,
    pub surface: Surface,
}

impl TelemetryOptions {
    /// Options from the owner's saved trace settings.
    pub fn from_trace(trace: crate::trace::TraceOptions, surface: Surface) -> Self {
        Self {
            traces: trace.enabled,
            content: trace.content,
            retention: Retention::default(),
            surface,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Chat,
    Agent,
    Subagent,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Agent => "agent",
            Self::Subagent => "subagent",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutcome {
    Ok,
    Error,
    Denied,
    Timeout,
}

impl ToolOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Denied => "denied",
            Self::Timeout => "timeout",
        }
    }

    /// The outcome of a finished tool call.
    pub fn of(result: &Result<String, crate::tools::ToolError>) -> Self {
        match result {
            Ok(_) => Self::Ok,
            Err(crate::tools::ToolError::Denied) => Self::Denied,
            Err(crate::tools::ToolError::Execution(message))
                if message == crate::tools::TIME_LIMIT_MESSAGE =>
            {
                Self::Timeout
            }
            Err(_) => Self::Error,
        }
    }
}

/// The tool name to record: the model's name only when it named a tool it was offered, so made-up
/// names (model-written text) never reach traces or metrics. Such calls are always denied.
pub fn tool_label(name: &str, offered: bool) -> &str {
    if offered {
        name
    } else {
        "unknown"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOutcome {
    Complete,
    Error,
    Cancelled,
    Iterations,
    Denied,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Iterations => "iterations",
            Self::Denied => "denied",
        }
    }
}

/// The token-usage buckets the GenAI conventions recommend.
const TOKEN_BUCKETS: [f64; 14] = [
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];
/// The operation-duration buckets the GenAI conventions recommend, in seconds.
const DURATION_BUCKETS: [f64; 14] = [
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];

/// Output-speed buckets, in tokens per second.
const SPEED_BUCKETS: [f64; 12] = [
    1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0, 2048.0,
];

struct Instruments {
    tokens: Histogram<u64>,
    duration: Histogram<f64>,
    speed: Histogram<f64>,
    saved: Histogram<u64>,
    tools: Counter<u64>,
    runs: Counter<u64>,
    dropped_runs: Counter<u64>,
}

/// The `blackwall.project` value for a workspace: its folder name only, never the path.
pub fn project_label(workspace: &Path) -> Option<String> {
    let text = workspace.to_string_lossy();
    let name = text
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty() && !name.ends_with(':'))?;
    Some(name.chars().take(128).collect())
}

/// A conversation ID is recorded only when it is a plain neutral ID.
fn session_label(session: &str) -> Option<String> {
    let plain = !session.is_empty()
        && session.len() <= 128
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    plain.then(|| session.to_owned())
}

struct Inner {
    directory: PathBuf,
    options: Mutex<TelemetryOptions>,
    tracer_provider: SdkTracerProvider,
    tracer: SdkTracer,
    meter_provider: SdkMeterProvider,
    instruments: Instruments,
    runs: Runs,
    exporter: export::Exporter,
    /// The export settings last applied, so an unchanged setting is not re-read every run.
    export_requested: Mutex<Option<ExportConfig>>,
    surface: Surface,
}

/// The process's telemetry. Cheap to clone; every clone shares the same providers.
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

fn scope() -> InstrumentationScope {
    InstrumentationScope::builder("blackwall")
        .with_version(env!("CARGO_PKG_VERSION"))
        .with_schema_url(names::SCHEMA_URL)
        .build()
}

impl Telemetry {
    /// Build the providers for `data_dir` and remove metrics files past the retention window.
    pub fn start(data_dir: &Path, options: TelemetryOptions) -> Self {
        file_metrics::prune(
            data_dir,
            options.retention,
            file_metrics::day_of(SystemTime::now()),
        );
        // An empty builder skips the SDK's environment-variable resource detectors.
        let resource = Resource::builder_empty()
            .with_schema_url(
                [
                    KeyValue::new(names::SERVICE_NAME, "blackwall"),
                    KeyValue::new(names::SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
                    KeyValue::new(names::SURFACE, options.surface.as_str()),
                ],
                names::SCHEMA_URL,
            )
            .build();
        let runs = Runs::default();
        let exporter = export::Exporter::default();
        let reader = PeriodicReader::builder(file_metrics::FileMetricExporter::new(
            data_dir,
            exporter.clone(),
        ))
        .with_interval(Duration::from_secs(60))
        .build();
        let meter_resource = resource.clone();
        let meter_provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(reader)
            .build();
        let meter = meter_provider.meter_with_scope(scope());
        let instruments = Instruments {
            tokens: meter
                .u64_histogram(names::GEN_AI_CLIENT_TOKEN_USAGE)
                .with_unit("{token}")
                .with_description("Number of input and output tokens used.")
                .with_boundaries(TOKEN_BUCKETS.to_vec())
                .build(),
            duration: meter
                .f64_histogram(names::GEN_AI_CLIENT_OPERATION_DURATION)
                .with_unit("s")
                .with_description("GenAI operation duration.")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            tools: meter
                .u64_counter(names::TOOL_INVOCATIONS)
                .with_unit("{call}")
                .with_description("Tool calls by outcome.")
                .build(),
            runs: meter
                .u64_counter(names::RUN_OUTCOMES)
                .with_unit("{run}")
                .with_description("Finished runs by outcome.")
                .build(),
            speed: meter
                .f64_histogram(names::OUTPUT_SPEED)
                .with_unit("{token}/s")
                .with_description("Output tokens per second of each model call.")
                .with_boundaries(SPEED_BUCKETS.to_vec())
                .build(),
            saved: meter
                .u64_histogram(names::COMPACTION_SAVED)
                .with_unit("{token}")
                .with_description("Estimated context tokens removed by each compaction.")
                .with_boundaries(TOKEN_BUCKETS.to_vec())
                .build(),
            dropped_runs: meter
                .u64_counter(names::DROPPED_RUNS)
                .with_unit("{run}")
                .with_description("Runs whose trace was dropped by a full writer queue.")
                .build(),
        };
        let tracer_provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .with_span_limits(SpanLimits::default())
            .with_resource(meter_resource)
            .with_span_processor(RunBufferProcessor::new(
                runs.clone(),
                instruments.dropped_runs.clone(),
                exporter.clone(),
            ))
            .build();
        let tracer = tracer_provider.tracer_with_scope(scope());
        Self {
            inner: Arc::new(Inner {
                directory: data_dir.to_path_buf(),
                options: Mutex::new(options),
                tracer_provider,
                tracer,
                meter_provider,
                instruments,
                runs,
                exporter,
                export_requested: Mutex::new(None),
                surface: options.surface,
            }),
        }
    }

    /// Apply the owner's export settings, reading header values from the credential store.
    /// Unchanged settings are kept without a credential-store read. Blocking.
    pub fn set_export(&self, config: &ExportConfig) {
        let unchanged = self.inner.export_requested.lock().is_ok_and(|requested| {
            requested.as_ref() == Some(config) || (requested.is_none() && !config.is_on())
        });
        if !unchanged {
            self.reload_export(config, &export::keychain_header);
        }
    }

    /// Apply export settings and read every header value again, such as after one was changed.
    /// A setting that fails to apply turns export off and is shown in the export status.
    pub fn reload_export(&self, config: &ExportConfig, lookup: &export::HeaderLookup<'_>) {
        if let Ok(mut requested) = self.inner.export_requested.lock() {
            *requested = Some(config.clone());
        }
        let applied = config.validated().and_then(|config| {
            let headers = if config.is_on() {
                export::resolve_headers(&config, lookup)?
            } else {
                Default::default()
            };
            Ok((config, headers))
        });
        match applied {
            Ok((config, _)) if !config.is_on() => self.inner.exporter.configure(None),
            Ok(settings) => self.inner.exporter.configure(Some(settings)),
            Err(error) => self.inner.exporter.refuse(config, &error),
        }
    }

    /// What the export has sent, dropped, and last failed with, since this process started.
    pub fn export_status(&self) -> ExportStatus {
        self.inner.exporter.status()
    }

    /// Send a test span with `config` (header values from the credential store) and record the
    /// outcome in the export status. Returns the HTTP status.
    pub async fn send_test_span(&self, config: &ExportConfig) -> Result<u16, ExportError> {
        let result = export::send_test_span(config, self.inner.surface.as_str()).await;
        self.inner.exporter.record_test(&result);
        result
    }

    /// Apply the owner's current trace settings to runs started from now on.
    pub fn set_options(&self, trace: crate::trace::TraceOptions) {
        if let Ok(mut options) = self.inner.options.lock() {
            options.traces = trace.enabled;
            options.content = trace.content;
        }
    }

    /// Start a run. With telemetry off the scope records nothing.
    pub fn run(&self, request_id: &str, mode: Mode) -> RunScope {
        let options = self
            .inner
            .options
            .lock()
            .map(|options| *options)
            .unwrap_or_default();
        if !options.traces {
            return RunScope::inert(mode);
        }
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        let safe: String = request_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .take(64)
            .collect();
        let path = self
            .inner
            .directory
            .join(crate::trace::TRACE_DIRECTORY)
            .join(format!("{started:013}-{safe}.json"));
        let (name, mut attributes) = match mode {
            Mode::Chat => ("chat".to_owned(), vec![]),
            Mode::Agent | Mode::Subagent => (
                "invoke_agent blackwall".to_owned(),
                vec![
                    KeyValue::new(names::GEN_AI_OPERATION_NAME, "invoke_agent"),
                    KeyValue::new(names::GEN_AI_AGENT_NAME, "blackwall"),
                ],
            ),
        };
        attributes.push(KeyValue::new(names::REQUEST_ID, request_id.to_owned()));
        attributes.push(KeyValue::new(names::MODE, mode.as_str()));
        let root = self
            .inner
            .tracer
            .span_builder(name)
            .with_kind(SpanKind::Internal)
            .with_attributes(attributes)
            .start_with_context(&self.inner.tracer, &Context::new());
        let context = root.span_context().clone();
        let run = Arc::new(RunState::new(path, options.content));
        self.inner.runs.insert(context.trace_id(), run.clone());
        let mut open = HashMap::new();
        open.insert(
            context.span_id(),
            Open {
                span: root,
                has_error_type: false,
            },
        );
        RunScope {
            telemetry: Some(self.clone()),
            run: Some(run),
            context: Some(context.clone()),
            mode,
            root: SpanId(Some(context.span_id())),
            open: Mutex::new(open),
            root_error: Mutex::new(None),
            count: AtomicUsize::new(1),
            finished: false,
            labels: vec![],
        }
    }

    /// Wait for queued trace files and write any metrics not yet on disk, then up to 3 s for
    /// export. Blocking: for exit paths and tests, not for the end of a run.
    pub fn flush(&self) {
        let _ = self.inner.tracer_provider.force_flush();
        let _ = self.inner.meter_provider.force_flush();
        self.inner.exporter.drain(export::DRAIN);
    }

    /// A scope for a compaction request: its model calls count toward token usage and duration,
    /// but it writes no trace and records no run outcome. With telemetry off it records nothing.
    pub fn compaction(&self, mode: Mode) -> RunScope {
        let traces = self
            .inner
            .options
            .lock()
            .map(|options| options.traces)
            .unwrap_or_default();
        let mut scope = RunScope::inert(mode);
        if traces {
            scope.telemetry = Some(self.clone());
        }
        scope
    }

    /// Flush and stop both providers, then wait up to 3 s for export. Runs still open afterwards
    /// are not written.
    pub fn shutdown(self) {
        let _ = self.inner.tracer_provider.shutdown();
        let _ = self.inner.meter_provider.shutdown();
        self.inner.exporter.stop(export::DRAIN);
    }
}

/// A model call in progress, from `RunScope::begin_model_call`.
pub struct ModelCall {
    span: SpanId,
    model: String,
    started: std::time::Instant,
    compaction: bool,
}

/// What a finished model call returned.
pub struct ModelReply<'a> {
    pub usage: Option<&'a TokenUsage>,
    pub tool_calls: bool,
    pub content: &'a str,
    /// Kept apart from `content`; recorded only with content capture on.
    pub reasoning: &'a str,
}

/// A failed model call: its error class and message.
pub struct ModelFailure<'a> {
    pub kind: &'a str,
    pub message: &'a str,
}

impl<'a> ModelFailure<'a> {
    pub fn of(error: &'a crate::model::ModelError, message: &'a str) -> Self {
        Self {
            kind: error.kind(),
            message,
        }
    }
}

impl<'a> ModelReply<'a> {
    pub fn of(turn: &'a crate::model::ModelTurn) -> Self {
        Self {
            usage: turn.usage.as_ref(),
            tool_calls: !turn.calls.is_empty(),
            content: &turn.content,
            reasoning: &turn.reasoning,
        }
    }
}

/// A started span; pass it back to `end`. A span over the run's limit has no ID and is ignored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpanId(Option<opentelemetry::trace::SpanId>);

struct Open {
    span: opentelemetry_sdk::trace::Span,
    has_error_type: bool,
}

/// One run's spans and metrics. Spans are buffered until the run finishes or is dropped; a
/// dropped scope is written as cancelled.
pub struct RunScope {
    telemetry: Option<Telemetry>,
    run: Option<Arc<RunState>>,
    context: Option<SpanContext>,
    mode: Mode,
    root: SpanId,
    open: Mutex<HashMap<opentelemetry::trace::SpanId, Open>>,
    root_error: Mutex<Option<String>>,
    count: AtomicUsize,
    finished: bool,
    /// `blackwall.project` and `blackwall.session`, added to this run's metrics.
    labels: Vec<KeyValue>,
}

fn otel_value(value: Value) -> opentelemetry::Value {
    match value {
        Value::String(text) => text.into(),
        Value::Bool(flag) => flag.into(),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.into(),
            None => number.as_f64().unwrap_or_default().into(),
        },
        Value::Array(items) if items.iter().all(Value::is_string) => {
            opentelemetry::Value::Array(Array::String(
                items
                    .into_iter()
                    .filter_map(|item| item.as_str().map(|text| StringValue::from(text.to_owned())))
                    .collect(),
            ))
        }
        other => other.to_string().into(),
    }
}

fn key_values(attributes: Vec<(String, Value)>) -> Vec<KeyValue> {
    attributes
        .into_iter()
        .map(|(key, value)| KeyValue::new(key, otel_value(value)))
        .collect()
}

fn span_kind(kind: u8) -> SpanKind {
    match kind {
        2 => SpanKind::Server,
        3 => SpanKind::Client,
        4 => SpanKind::Producer,
        5 => SpanKind::Consumer,
        _ => SpanKind::Internal,
    }
}

/// The host and port of an endpoint URL or a bare `host:port`. Anything else, such as a socket
/// path, has no host, so no `server.address` is recorded.
fn server_parts(server: &str) -> (Option<String>, Option<u16>) {
    if let Ok(url) = reqwest::Url::parse(server) {
        if let Some(host) = url.host_str().filter(|host| !host.is_empty()) {
            return (Some(host.to_owned()), url.port_or_known_default());
        }
    }
    let Some((host, port)) = server.rsplit_once(':') else {
        return (None, None);
    };
    let plain = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    match port.parse::<u16>() {
        Ok(port) if plain => (Some(host.to_owned()), Some(port)),
        _ => (None, None),
    }
}

impl RunScope {
    fn inert(mode: Mode) -> Self {
        Self {
            telemetry: None,
            run: None,
            context: None,
            mode,
            root: SpanId(None),
            open: Mutex::new(HashMap::new()),
            root_error: Mutex::new(None),
            count: AtomicUsize::new(0),
            finished: true,
            labels: vec![],
        }
    }

    /// Label this run's metrics with its project (the workspace folder's name, never its path)
    /// and its neutral conversation ID. Chat runs pass no workspace.
    pub fn labelled(mut self, workspace: Option<&Path>, session: Option<&str>) -> Self {
        self.labels.clear();
        if let Some(project) = workspace.and_then(project_label) {
            self.labels.push(KeyValue::new(names::PROJECT, project));
        }
        if let Some(session) = session.and_then(session_label) {
            self.labels.push(KeyValue::new(names::SESSION, session));
        }
        self
    }

    /// Record the estimated tokens one compaction removed from the context.
    pub fn record_compaction_saved(&self, tokens: u64) {
        if let Some(telemetry) = &self.telemetry {
            let mut attributes = vec![KeyValue::new(names::MODE, self.mode.as_str())];
            attributes.extend(self.labels.iter().cloned());
            telemetry
                .inner
                .instruments
                .saved
                .record(tokens, &attributes);
        }
    }

    /// Whether prompts, outputs, and tool data may be recorded.
    pub fn content(&self) -> bool {
        self.run.as_ref().is_some_and(|run| run.content)
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The run's root span.
    pub fn root(&self) -> SpanId {
        self.root.clone()
    }

    /// Start a child span. `kind` follows OTLP: 1 internal, 3 client.
    pub fn begin(
        &self,
        parent: &SpanId,
        name: &str,
        kind: u8,
        attributes: Vec<(String, Value)>,
    ) -> SpanId {
        let (Some(telemetry), Some(run), Some(context), Some(parent)) =
            (&self.telemetry, &self.run, &self.context, parent.0)
        else {
            return SpanId(None);
        };
        if self.count.fetch_add(1, Ordering::Relaxed) >= MAX_SPANS {
            run.dropped.fetch_add(1, Ordering::Relaxed);
            return SpanId(None);
        }
        let has_error_type = attributes.iter().any(|(key, _)| key == names::ERROR_TYPE);
        let parent = Context::new().with_remote_span_context(SpanContext::new(
            context.trace_id(),
            parent,
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        ));
        let tracer = &telemetry.inner.tracer;
        let span = tracer
            .span_builder(name.to_owned())
            .with_kind(span_kind(kind))
            .with_attributes(key_values(attributes))
            .start_with_context(tracer, &parent);
        let id = span.span_context().span_id();
        if let Ok(mut open) = self.open.lock() {
            open.insert(
                id,
                Open {
                    span,
                    has_error_type,
                },
            );
        }
        SpanId(Some(id))
    }

    fn with_span(&self, id: &SpanId, change: impl FnOnce(&mut Open)) {
        let Some(id) = id.0 else {
            return;
        };
        if let Ok(mut open) = self.open.lock() {
            if let Some(span) = open.get_mut(&id) {
                change(span);
            }
        }
    }

    /// Add attributes to a span that is still open.
    pub fn set(&self, id: &SpanId, attributes: Vec<(String, Value)>) {
        self.with_span(id, |open| {
            open.has_error_type |= attributes.iter().any(|(key, _)| key == names::ERROR_TYPE);
            for pair in key_values(attributes) {
                open.span.set_attribute(pair);
            }
        });
    }

    pub fn event(&self, id: &SpanId, name: &str, attributes: Vec<(String, Value)>) {
        self.with_span(id, |open| {
            open.span.add_event(name.to_owned(), key_values(attributes));
        });
    }

    /// Rename a span, such as a Chat run's root once its model is known.
    pub fn rename(&self, id: &SpanId, name: &str) {
        self.with_span(id, |open| open.span.update_name(name.to_owned()));
    }

    /// Redacted, bounded text for a content attribute. Secret values it removes are also removed
    /// from the rest of the run when it is written, such as when the model repeats one.
    pub fn clip(&self, text: &str) -> String {
        let mut found = vec![];
        let text = redact::clip(text, &mut found);
        if let Some(run) = &self.run {
            if let Ok(mut secrets) = run.secrets.lock() {
                secrets.extend(found.into_iter().filter(|value| value.chars().count() >= 6));
            }
        }
        text
    }

    /// End a span; an error message marks it failed. With content capture off only the span's
    /// `error.type` is kept as the message. The root ends when the run finishes.
    pub fn end(&self, id: &SpanId, error: Option<&str>) {
        let error = error.map(|error| self.clip(error));
        if *id == self.root {
            if let (Some(error), Ok(mut root)) = (error, self.root_error.lock()) {
                *root = Some(error);
            }
            return;
        }
        let Some(key) = id.0 else {
            return;
        };
        let open = self.open.lock().ok().and_then(|mut open| open.remove(&key));
        if let Some(open) = open {
            close(open, error);
        }
    }

    /// Start a `chat {model}` span for one model call under `parent`.
    pub fn begin_model_call(&self, parent: &SpanId, model: &str) -> ModelCall {
        ModelCall {
            span: self.begin(
                parent,
                &format!("chat {model}"),
                3,
                vec![
                    (names::GEN_AI_OPERATION_NAME.into(), Value::from("chat")),
                    (names::GEN_AI_REQUEST_MODEL.into(), Value::from(model)),
                ],
            ),
            model: model.to_owned(),
            started: std::time::Instant::now(),
            compaction: false,
        }
    }

    /// Start a model call that summarizes history for compaction. It is a `chat` operation, as
    /// the conventions define it, marked `blackwall.compaction=true`; its summary is not recorded.
    pub fn begin_compaction_call(&self, model: &str) -> ModelCall {
        let mut call = self.begin_model_call(&self.root(), model);
        self.set(
            &call.span,
            vec![(names::COMPACTION.into(), Value::from(true))],
        );
        call.compaction = true;
        call
    }

    /// End a model call's span and record its metrics for `mode`. A reply's text is kept only
    /// with content capture on; a failure keeps its error class.
    pub fn end_model_call(
        &self,
        call: ModelCall,
        mode: Mode,
        server: &str,
        result: Result<ModelReply<'_>, ModelFailure<'_>>,
    ) {
        let seconds = call.started.elapsed().as_secs_f64();
        let usage = result.as_ref().ok().and_then(|reply| reply.usage);
        let kind = result.as_ref().err().map(|failure| failure.kind);
        self.record_model_metrics(
            mode,
            &call.model,
            server,
            usage,
            seconds,
            kind,
            call.compaction,
        );
        match result {
            Ok(reply) => {
                let mut attributes = vec![(
                    names::GEN_AI_RESPONSE_FINISH_REASONS.into(),
                    serde_json::json!([if reply.tool_calls {
                        "tool_calls"
                    } else {
                        "stop"
                    }]),
                )];
                if let Some(usage) = reply.usage {
                    attributes.push((
                        names::GEN_AI_USAGE_INPUT_TOKENS.into(),
                        Value::from(usage.prompt_tokens),
                    ));
                    attributes.push((
                        names::GEN_AI_USAGE_OUTPUT_TOKENS.into(),
                        Value::from(usage.completion_tokens),
                    ));
                }
                if self.content()
                    && !call.compaction
                    && !(reply.content.is_empty() && reply.reasoning.is_empty())
                {
                    let mut parts = Vec::new();
                    if !reply.reasoning.is_empty() {
                        parts.push(serde_json::json!({"type":"reasoning","content":self.clip(reply.reasoning)}));
                    }
                    if !reply.content.is_empty() {
                        parts.push(
                            serde_json::json!({"type":"text","content":self.clip(reply.content)}),
                        );
                    }
                    let parts = serde_json::json!([{"role":"assistant","parts":parts}]);
                    attributes.push((
                        names::GEN_AI_OUTPUT_MESSAGES.into(),
                        Value::from(parts.to_string()),
                    ));
                }
                self.set(&call.span, attributes);
                self.end(&call.span, None);
            }
            Err(failure) => {
                self.set(
                    &call.span,
                    vec![(names::ERROR_TYPE.into(), Value::from(failure.kind))],
                );
                self.end(&call.span, Some(failure.message));
            }
        }
    }

    /// Record one model call's tokens and duration, attributed to this run's mode.
    pub fn record_model_call(
        &self,
        model: &str,
        server: &str,
        usage: Option<&TokenUsage>,
        seconds: f64,
        error: Option<&str>,
    ) {
        self.record_model_call_as(self.mode, model, server, usage, seconds, error);
    }

    /// Record one model call for another mode in this run, such as a subagent's.
    pub fn record_model_call_as(
        &self,
        mode: Mode,
        model: &str,
        server: &str,
        usage: Option<&TokenUsage>,
        seconds: f64,
        error: Option<&str>,
    ) {
        self.record_model_metrics(mode, model, server, usage, seconds, error, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn record_model_metrics(
        &self,
        mode: Mode,
        model: &str,
        server: &str,
        usage: Option<&TokenUsage>,
        seconds: f64,
        error: Option<&str>,
        compaction: bool,
    ) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        let (host, port) = server_parts(server);
        let mut attributes = vec![
            KeyValue::new(names::GEN_AI_OPERATION_NAME, "chat"),
            KeyValue::new(names::GEN_AI_REQUEST_MODEL, model.to_owned()),
            KeyValue::new(
                names::GEN_AI_PROVIDER_NAME,
                if port == Some(11434) {
                    "ollama"
                } else {
                    "openai_compatible"
                },
            ),
            KeyValue::new(names::MODE, mode.as_str()),
        ];
        if let Some(host) = host {
            attributes.push(KeyValue::new(names::SERVER_ADDRESS, host));
        }
        if compaction {
            attributes.push(KeyValue::new(names::COMPACTION, true));
        }
        attributes.extend(self.labels.iter().cloned());
        let instruments = &telemetry.inner.instruments;
        if let Some(usage) = usage {
            // `cached_input` and `reasoning` are parts of `input` and `output`, recorded only
            // when the server reports them.
            for (kind, count, always) in [
                ("input", usage.prompt_tokens, true),
                ("output", usage.completion_tokens, true),
                ("cached_input", usage.cached_input_tokens, false),
                ("reasoning", usage.reasoning_tokens, false),
            ] {
                if always || count > 0 {
                    let mut attributes = attributes.clone();
                    attributes.push(KeyValue::new(names::GEN_AI_TOKEN_TYPE, kind));
                    instruments.tokens.record(count, &attributes);
                }
            }
            if error.is_none() && usage.completion_tokens > 0 && seconds > 0.0 {
                instruments
                    .speed
                    .record(usage.completion_tokens as f64 / seconds, &attributes);
            }
        }
        if let Some(error) = error {
            attributes.push(KeyValue::new(names::ERROR_TYPE, error.to_owned()));
        }
        instruments.duration.record(seconds.max(0.0), &attributes);
    }

    /// Count one tool call. MCP tools are named `server__tool`.
    pub fn record_tool(&self, name: &str, outcome: ToolOutcome) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.inner.instruments.tools.add(
                1,
                &[
                    KeyValue::new(names::GEN_AI_TOOL_NAME, name.to_owned()),
                    KeyValue::new(names::TOOL_OUTCOME, outcome.as_str()),
                ],
            );
        }
    }

    /// End the run: spans still open are marked cancelled, the root ends with the outcome, the
    /// buffered run is written, and its metrics are flushed.
    pub fn finish(mut self, outcome: RunOutcome) {
        self.close_run(outcome);
    }

    fn close_run(&mut self, outcome: RunOutcome) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let mut open = self
            .open
            .lock()
            .map(|mut open| std::mem::take(&mut *open))
            .unwrap_or_default();
        let root = self.root.0.and_then(|id| open.remove(&id));
        // A span still open when the run ended was cut off.
        for (_, mut span) in open {
            if !span.has_error_type {
                span.span
                    .set_attribute(KeyValue::new(names::ERROR_TYPE, "cancelled"));
                span.has_error_type = true;
            }
            close(span, Some("Cancelled.".into()));
        }
        if let Some(mut root) = root {
            let saved = self
                .root_error
                .lock()
                .ok()
                .and_then(|mut error| error.take());
            let (kind, fallback) = match outcome {
                RunOutcome::Complete => (None, None),
                RunOutcome::Cancelled => (Some("cancelled"), Some("The run was stopped.")),
                RunOutcome::Iterations => (Some("iterations"), Some("The run reached its limit.")),
                RunOutcome::Denied => (Some("denied"), Some("The run was denied.")),
                RunOutcome::Error => (None, Some("The run failed.")),
            };
            let error = saved.or_else(|| fallback.map(str::to_owned));
            if let (Some(kind), false) = (kind, root.has_error_type) {
                root.span
                    .set_attribute(KeyValue::new(names::ERROR_TYPE, kind));
                root.has_error_type = true;
            }
            close(root, error);
        }
        if let Some(telemetry) = &self.telemetry {
            let mut attributes = vec![
                KeyValue::new(names::MODE, self.mode.as_str()),
                KeyValue::new(names::RUN_OUTCOME, outcome.as_str()),
            ];
            attributes.extend(self.labels.iter().cloned());
            telemetry.inner.instruments.runs.add(1, &attributes);
        }
    }
}

fn close(mut open: Open, error: Option<String>) {
    match error {
        Some(message) => {
            if !open.has_error_type {
                open.span
                    .set_attribute(KeyValue::new(names::ERROR_TYPE, "_OTHER"));
            }
            open.span.set_status(Status::error(message));
        }
        None => open.span.set_status(Status::Ok),
    }
    open.span.end();
}

impl Drop for RunScope {
    fn drop(&mut self) {
        self.close_run(RunOutcome::Cancelled);
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
